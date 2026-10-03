package tech.yaya.agente

import android.app.Notification
import android.app.PendingIntent
import android.app.RemoteInput
import android.content.ComponentName
import android.content.Intent
import android.os.Bundle
import android.service.notification.NotificationListenerService
import android.service.notification.StatusBarNotification
import android.util.Log
import androidx.core.app.NotificationCompat

/**
 * Listens to incoming notifications, parses the ones from supported messaging
 * apps, and answers them through the notification's own inline-reply
 * (RemoteInput) action — the same mechanism Android Wear / Auto use.
 * Nothing is scraped from the apps themselves; only what they publish in the
 * notification shade is read, and only after the user grants Notification
 * Access to agente in system settings.
 */
class AgenteNotificationListener : NotificationListenerService() {

    /** conversationKey -> last auto-reply epoch millis (cooldown). */
    private val lastReplied = HashMap<String, Long>()

    /** Recently handled payment-notification identities, to ignore reposts.
     *  Chat messages are deduped durably by [InboxQueue] instead. */
    private val handled = LinkedHashMap<String, Long>()

    // ------------------------------------------------------ listener lifecycle

    override fun onListenerConnected() {
        super.onListenerConnected()
        Log.i(TAG, "listener connected")
        ServerClient.IO_EXECUTOR.execute {
            runCatching { AgenteCore.ensureStarted(applicationContext) }
            runCatching { Prefs.refreshLearnedSources(applicationContext) }
        }
        runCatching { YapeCollector.catchUp(applicationContext, activeNotifications) }
    }

    /**
     * The system (or an OEM battery manager — MIUI/EMUI are notorious for
     * killing listener bindings) dropped us. requestRebind is the one call
     * documented as safe after onListenerDisconnected; asking for a rebind
     * costs nothing when the user simply revoked access, and brings the agent
     * back without user action when the disconnect was a background kill.
     */
    override fun onListenerDisconnected() {
        super.onListenerDisconnected()
        Log.w(TAG, "listener disconnected — requesting rebind")
        try {
            requestRebind(ComponentName(this, AgenteNotificationListener::class.java))
        } catch (t: Throwable) {
            Log.e(TAG, "requestRebind failed", t)
        }
    }

    override fun onNotificationPosted(sbn: StatusBarNotification?) {
        // Each notification is processed under its own catch: a malformed one
        // (OEM-mangled extras, a broken PendingIntent, a server hiccup) must
        // never take the callback down and cost us the messages after it.
        if (sbn == null) return
        try {
            process(sbn)
        } catch (t: Throwable) {
            Log.e(TAG, "error processing notification from ${sbn.packageName}", t)
        }
    }

    private fun process(sbn: StatusBarNotification) {
        val ctx = applicationContext
        // The till runs whether or not the agent answers chats on this phone.
        runCatching { YapeCollector.onPosted(ctx, sbn) }
        if (!Prefs.isEnabled(ctx)) return

        val pkg = sbn.packageName
        // The owner's word before anything else (the read screen): an app
        // they did not let the agent read is dropped here, unparsed and
        // unlogged — the chat path, the payment reader and the observer that
        // learns new apps never see it.
        if (!Prefs.canRead(ctx, pkg)) return

        // Built-in apps first; then the ones this phone taught itself
        // (ProfileStore). Anything else is either a chat app we are about to
        // learn (it carries an inline reply) or a candidate money notification.
        val profile = if (SupportedApps.isSupported(pkg)) null else ProfileStore.get(ctx, pkg)
        val app = SupportedApps.get(pkg) ?: profile?.let { SupportedApp(it.packageName, it.displayName) }
        if (app == null) {
            if (UnknownAppObserver.observe(ctx, sbn) { parseConversation(it) != null }) return
            PaymentDetector.inspect(ctx, this, sbn)?.let { hit ->
                handlePaymentNotification(hit, sbn)
            }
            return
        }
        // A learned app is read even while its reply switch is off: that is
        // how it earns the switch. A built-in app the owner lets the agent
        // read but not answer is recorded below and left for the owner.
        val live = profile == null || ProfileStore.isLive(ctx, profile)
        val answers = profile != null || Prefs.isAppEnabled(ctx, app.packageName)

        val n = sbn.notification ?: return

        // Skip summaries, ongoing/foreground-service notifications, and anything
        // that isn't a fresh message.
        if (n.flags and Notification.FLAG_GROUP_SUMMARY != 0) return
        if (n.flags and Notification.FLAG_ONGOING_EVENT != 0) return

        val conv = parseConversation(n)
        if (profile != null) {
            // Every read is evidence for the trial: a clean parse with a
            // reply action counts for the profile, anything else against it.
            val readOk = conv != null && findReplyAction(n) != null
            ProfileStore.record(ctx, profile.packageName, readOk)?.let { (p, pausedNow) ->
                if (pausedNow) {
                    ReplyLog.add(ctx, ReplyEvent(
                        timestamp = System.currentTimeMillis(), appPackage = p.packageName, appName = p.displayName,
                        sender = p.displayName, incomingText = "", replySent = false,
                        detail = ctx.getString(R.string.log_learned_paused, p.displayName)
                    ))
                    OwnerAlerts.notify(ctx, urgent = false, sender = p.displayName,
                        question = ctx.getString(R.string.learned_paused_question, p.displayName),
                        gapId = "profile:${p.packageName}")
                }
            }
            if (!live) {
                if (conv != null && conv.inbound.isNotEmpty()) {
                    log(app, conv.asMessage(), sent = false, detail = ctx.getString(R.string.log_shadow))
                }
                return
            }
        }
        if (conv == null) return

        // Nothing but our own outgoing messages echoed back into the
        // conversation notification (MessagingStyle marks those as coming from
        // the device user) — there is no customer turn here to answer.
        if (conv.inbound.isEmpty()) return

        // Group chats are opt-in for businesses.
        if (conv.isGroup && !Prefs.replyToGroups(ctx)) {
            log(app, conv.asMessage(), sent = false, detail = ctx.getString(R.string.log_skipped_group))
            return
        }

        // Who this is, in the one shape the CRM and the agent's memory use.
        val ref = SenderRef.from(Channel.of(pkg, app.displayName), conv.sender)

        // Write every message down before doing anything about it, and let the
        // queue tell us which ones are genuinely new. This replaces the old
        // in-memory repost guard: apps re-post a notification whenever the
        // unread count changes, replaying messages we have already answered,
        // and the queue's unique index absorbs those durably — across a process
        // death, which a HashMap did not.
        val fresh = InboxQueue.addNew(
            ctx,
            conv.inbound.map { m ->
                InboxQueue.Item(
                    channel = ref.channel.id,
                    handle = ref.handle,
                    displayName = ref.displayName,
                    appPackage = pkg,
                    text = m.text,
                    sentAt = m.sentAt,
                    isGroup = conv.isGroup,
                    notifKey = sbn.key,
                )
            }
        )
        if (fresh.isEmpty()) return

        if (!answers) {
            // Read, not answered: the owner let the agent read this app but
            // not reply on it. The messages stay on record for the owner,
            // settled, so switching replies on later never answers old ones.
            InboxQueue.settle(ctx, fresh.map { it.id }, InboxQueue.SKIPPED)
            log(app, ParsedMessage(ref.displayName, fresh.joinToString("\n") { it.text }, conv.isGroup),
                sent = false, detail = ctx.getString(R.string.log_read_only))
            return
        }

        // Whatever this customer said while we were deaf — core unreachable, a
        // bundle with no reply action, a process killed mid-turn — rides along
        // with what they just said, so the agent answers all of it together
        // instead of replying to the last line as if the rest never happened.
        val stranded = InboxQueue.pendingFor(
            ctx, ref.channel.id, ref.handle, excluding = fresh.map { it.id }
        )
        val turn = (stranded + fresh).sortedBy { it.sentAt }

        val ids = turn.map { it.id }
        // One turn carrying everything the customer has said since we last
        // answered. The core's own note on this is explicit: several
        // notifications belong together as user turns reconciled against the
        // stored log, never as a replacement for it — so we send what they
        // said, joined in order, and let the core hold the history.
        val parsed = ParsedMessage(
            sender = ref.displayName,
            text = turn.joinToString("\n") { it.text },
            isGroup = conv.isGroup,
        )

        val replyAction = findReplyAction(n)
        if (replyAction == null) {
            // The bundle collapsed past the point of carrying a reply action.
            // The messages are already stored, so the owner still sees them and
            // a later notification from this chat can still be answered.
            InboxQueue.settle(ctx, ids, InboxQueue.SKIPPED)
            log(app, parsed, sent = false, detail = ctx.getString(R.string.log_no_reply_action))
            return
        }

        if (Prefs.serverConfigured(ctx)) {
            // Agent mode: every message goes to the server; the LLM agent holds
            // the conversation, so no cooldown — the queue's unique index
            // already blocks notification reposts. The catch keeps one bad
            // exchange from poisoning the shared executor thread, and leaves the
            // rows pending so they are still on record.
            ServerClient.EXECUTOR.execute {
                try {
                    agentReply(app, ref, parsed, replyAction, ids, sbn.key)
                } catch (t: Throwable) {
                    Log.e(TAG, "agent reply failed for ${app.packageName}", t)
                    InboxQueue.settle(ctx, ids, InboxQueue.FAILED)
                }
            }
            return
        }

        // Canned mode: per-conversation cooldown so a customer gets one
        // auto-reply, not one per message.
        val now = System.currentTimeMillis()
        val convKey = "${app.packageName}|${parsed.sender}"
        val cooldownMs = Prefs.cooldownMinutes(ctx) * 60_000L
        synchronized(lastReplied) {
            // Bounded memory: anything idle past the longest possible cooldown
            // (24 h, enforced by the Settings editor) can never gate again.
            lastReplied.entries.removeAll { now - it.value > MAX_COOLDOWN_MS }
            val last = lastReplied[convKey] ?: 0L
            if (now - last < cooldownMs) {
                // Answered recently: the customer is not owed another canned
                // line, but the message itself is read and accounted for.
                InboxQueue.settle(ctx, ids, InboxQueue.SKIPPED)
                log(app, parsed, sent = false, detail = ctx.getString(R.string.log_skipped_cooldown))
                return
            }
            lastReplied[convKey] = now
        }

        val replyText = Prefs.replyText(ctx)
        val ok = sendReply(replyAction, replyText)
        InboxQueue.settle(ctx, ids, if (ok) InboxQueue.REPLIED else InboxQueue.FAILED, replyText)
        if (ok) clearIfAnswered(sbn.key)
        log(app, parsed, sent = ok, detail = if (ok) replyText else ctx.getString(R.string.log_send_failed))
    }

    /**
     * Dismiss a notification the agent has answered, so the app's bundle starts
     * again from empty.
     *
     * This is the other half of reading a whole bundle: left alone, an
     * unanswered pile grows into "10 chats from 2 contacts", and eventually
     * into a summary carrying no reply action at all — at which point the agent
     * cannot answer that chat however well it parses. Only ever called after a
     * reply actually went out, so nothing the owner still has to read is
     * cleared from under them.
     */
    private fun clearIfAnswered(key: String?) {
        if (key == null || !Prefs.clearAfterReply(applicationContext)) return
        try {
            cancelNotification(key)
        } catch (t: Throwable) {
            // Losing the race with the app dismissing it first is normal and
            // costs nothing; the reply has already been sent.
            Log.w(TAG, "could not cancel notification", t)
        }
    }

    /** Any bank/wallet "money arrived": forward to the server for payment matching. */
    private fun handlePaymentNotification(hit: PaymentDetector.Hit, sbn: StatusBarNotification) {
        val app = SupportedApp(hit.packageName, hit.label)
        val title = hit.title
        val text = hit.text

        val identity = "${sbn.key}|$title|${text.hashCode()}"
        val now = System.currentTimeMillis()
        synchronized(handled) {
            handled.entries.removeAll { now - it.value > IDENTITY_WINDOW_MS }
            if (handled.containsKey(identity)) return
            handled[identity] = now
        }

        val parsed = ParsedMessage(sender = title, text = text, isGroup = false)
        val ctx = applicationContext
        if (!Prefs.serverConfigured(ctx)) {
            log(app, parsed, sent = false, detail = ctx.getString(R.string.log_payment_no_server))
            return
        }
        ServerClient.IO_EXECUTOR.execute {
            try {
                val resp = ServerClient.paymentEvent(ctx, hit.envelope)
                // The agent read it. "mute" = no money in this app; ask again later.
                if (resp?.optBoolean("mute") == true) {
                    Prefs.muteSource(ctx, app.packageName, resp.optLong("untilMs", System.currentTimeMillis() + 7L * 24 * 3600 * 1000))
                    return@execute
                }
                if (resp?.optBoolean("handled") != true) return@execute
                val detail = when {
                    !resp.isNull("matchedAppointment") ->
                        ctx.getString(R.string.log_payment_matched, resp.optJSONObject("matchedAppointment")?.optString("customer") ?: "")
                    !resp.isNull("matchedOrder") ->
                        ctx.getString(R.string.log_payment_matched, resp.optJSONObject("matchedOrder")?.optString("customer") ?: "")
                    else -> ctx.getString(R.string.log_payment_recorded)
                }
                log(app, parsed, sent = false, detail = "💰 $detail")
            } catch (t: Throwable) {
                Log.e(TAG, "payment forward failed for ${app.packageName}", t)
            }
        }
    }

    /** Runs on ServerClient.EXECUTOR: ask the server agent, then reply inline. */
    private fun agentReply(
        app: SupportedApp,
        ref: SenderRef,
        parsed: ParsedMessage,
        replyAction: Notification.Action,
        ids: List<Long>,
        notifKey: String?,
    ) {
        val ctx = applicationContext
        // Unchanged on purpose: message history, the audit chain and D14's
        // conversation_months all key on this string. The channel/handle
        // identity rides alongside it so the core can decide that two peers are
        // one person without any of that being rewritten underneath it.
        val peer = ref.legacyPeer(app.packageName)
        val resp = ServerClient.executeAction(ctx, peer, parsed.text, ref)
        // The server flags questions that need the owner (out-of-scope, or
        // the customer asked for a human) — raise a local heads-up for each.
        resp?.optJSONArray("attention")?.let { arr ->
            for (i in 0 until arr.length()) {
                // opt + per-item catch: a malformed gap must not stop the
                // remaining alerts nor the customer reply below.
                val g = arr.optJSONObject(i) ?: continue
                try {
                    OwnerAlerts.notify(
                        ctx,
                        urgent = g.optBoolean("urgent"),
                        sender = parsed.sender,
                        question = g.optString("question"),
                        gapId = g.optString("gapId", "gap$i")
                    )
                } catch (t: Throwable) {
                    Log.e(TAG, "owner alert failed", t)
                }
            }
        }
        // Manual mode: the account is past its grace floor, so the core did
        // not run the agent (docs/CREDITS.md § 2). The customer's message was
        // stored for the owner and rides in `attention` (raised above); the
        // agent sends NOTHING — the chat is never blocked, the owner answers
        // by hand. The owner is reminded at most every 6 h, with the top-up
        // link. Cores older than the credits model said "limit_reached" with
        // a holding line; that line is still sent once per hour per chat.
        val action = resp?.optString("action")
        if (action == "no_credits" || action == "limit_reached") {
            val holding = resp!!.optString("agentResponse").takeIf { it.isNotBlank() && it != "null" }
            val convKey = "limit|${app.packageName}|${parsed.sender}"
            val now = System.currentTimeMillis()
            val sendHolding = holding != null && synchronized(lastReplied) {
                lastReplied.entries.removeAll { now - it.value > MAX_COOLDOWN_MS }
                val fresh = now - (lastReplied[convKey] ?: 0L) > LIMIT_HOLDING_COOLDOWN_MS
                if (fresh) lastReplied[convKey] = now
                fresh
            }
            val sent = holding != null && sendHolding && sendReply(replyAction, holding)
            // Manual mode: the owner answers by hand, so the notification stays
            // in their shade whatever we did with the holding line — clearing
            // it would hide the very message they now have to deal with.
            InboxQueue.settle(ctx, ids, InboxQueue.SKIPPED, holding)
            log(app, parsed, sent = sent, detail = ctx.getString(R.string.log_manual_mode))
            if (now - lastCreditsAlert > CREDITS_ALERT_INTERVAL_MS) {
                lastCreditsAlert = now
                OwnerAlerts.notify(
                    ctx,
                    urgent = true,
                    sender = ctx.getString(R.string.app_name),
                    question = ctx.getString(R.string.manual_mode_alert),
                    gapId = "no_credits",
                    // No checkout links in the app (Google Play payments policy).
                    url = null
                )
            }
            return
        }

        val text = replyTextOf(resp)
        if (text != null) {
            val ok = sendReply(replyAction, text)
            InboxQueue.settle(ctx, ids, if (ok) InboxQueue.REPLIED else InboxQueue.FAILED, text)
            if (ok) clearIfAnswered(notifKey)
            val suffix = action?.takeIf { it.isNotEmpty() && it != "null" }?.let { " [$it]" } ?: ""
            log(app, parsed, sent = ok,
                detail = if (ok) "$text$suffix" else ctx.getString(R.string.log_send_failed))
        } else {
            // Server unreachable — fall back to the canned reply so the
            // customer still hears back. The rows stay PENDING: the agent never
            // saw these messages, and a canned line is not an answer to them.
            val ok = sendReply(replyAction, Prefs.replyText(ctx))
            log(app, parsed, sent = ok, detail = ctx.getString(R.string.log_fallback_sent))
        }
    }

    // ---------------------------------------------------------------- parsing

    internal data class ParsedMessage(
        val sender: String,
        val text: String,
        val isGroup: Boolean,
    )

    /** One inbound message. [sentAt] is what the queue dedupes on. */
    internal data class InMsg(val text: String, val sentAt: Long)

    /** A chat as one notification presents it, with every message it carries. */
    internal data class Conversation(
        val sender: String,
        val isGroup: Boolean,
        /** The customer's messages only, oldest first; our own are dropped. */
        val inbound: List<InMsg>,
    ) {
        /** The whole turn as one message, for logging and canned replies. */
        fun asMessage() = ParsedMessage(
            sender = sender,
            text = inbound.joinToString("\n") { it.text },
            isGroup = isGroup,
        )
    }

    /**
     * Reads **every** message a notification carries, not just the newest.
     *
     * This is the difference between answering a customer and losing them. Once
     * an app bundles — and Instagram and Messenger bundle aggressively — a
     * single notification stands in for everything unread on that chat. Taking
     * only `messages.last()` silently discarded the rest, so a customer who
     * sent "hola", then their order, then their address got answered as though
     * they had only ever sent the address.
     */
    internal fun parseConversation(n: Notification): Conversation? {
        val extras = n.extras ?: return null

        // Prefer MessagingStyle — WhatsApp, Messenger, Telegram and Google
        // Messages all use it and it cleanly separates sender / self / group.
        val style = NotificationCompat.MessagingStyle
            .extractMessagingStyleFromNotification(n)
        if (style != null && style.messages.isNotEmpty()) {
            val selfName = style.user.name?.toString()
            fun isSelf(p: androidx.core.app.Person?) =
                p == null || p.name.isNullOrEmpty() || p.name.toString() == selfName

            val inbound = style.messages
                .filterNot { isSelf(it.person) }
                .mapNotNull { m ->
                    val t = m.text?.toString()?.trim().orEmpty()
                    if (t.isEmpty()) null else InMsg(t, m.timestamp)
                }
                .sortedBy { it.sentAt }

            // In a group the conversation title names the room, not the person;
            // groups are opt-in and off by default, so collapsing a room to one
            // sender is the existing, deliberate behaviour.
            val sender = style.conversationTitle?.toString()
                ?: style.messages.lastOrNull { !isSelf(it.person) }?.person?.name?.toString()
                ?: extras.getCharSequence(Notification.EXTRA_TITLE)?.toString()
                ?: return null
            return Conversation(sender, style.isGroupConversation, inbound)
        }

        // Fallback: plain title/text notification.
        val title = extras.getCharSequence(Notification.EXTRA_TITLE)?.toString() ?: return null
        val text = extras.getCharSequence(Notification.EXTRA_TEXT)?.toString() ?: return null
        // "Checking for new messages", "X new messages" style placeholders.
        if (text.isBlank() || looksLikePlaceholder(text)) return null
        // No per-message timestamp here, and the queue dedupes on one. `when` is
        // the message's own time and stays put across the re-posts an app makes
        // when its unread count changes, which is exactly what we need. When an
        // app leaves it at 0 we bucket to the minute instead: a burst of
        // re-posts collapses to one row, while the same words sent again an hour
        // later are still read as a new message.
        val stamp = n.`when`.takeIf { it > 0L }
            ?: (System.currentTimeMillis() / 60_000L * 60_000L)
        return Conversation(
            sender = title,
            isGroup = extras.getBoolean(Notification.EXTRA_IS_GROUP_CONVERSATION, false),
            inbound = listOf(InMsg(text, stamp)),
        )
    }

    /**
     * System filler an app posts instead of a message ("3 new messages",
     * "Checking for new messages"). Matched against the *whole* text: a
     * customer who writes "checking for availability tomorrow" is a customer.
     */
    internal fun looksLikePlaceholder(text: String): Boolean =
        PLACEHOLDERS.any { it.matches(text.trim().lowercase()) }

    // ---------------------------------------------------------- reply sending

    internal fun findReplyAction(n: Notification): Notification.Action? {
        val direct = n.actions.orEmpty()
            .filterNotNull()
            .filter { !it.remoteInputs.isNullOrEmpty() }
        // Prefer an action explicitly marked/labelled as a reply; otherwise any
        // action carrying a RemoteInput is almost always the reply on messaging
        // notifications ("Mark as read" etc. carry no RemoteInput).
        direct.firstOrNull { isReplyLike(it) }?.let { return it }
        direct.firstOrNull()?.let { return it }
        // Some apps only expose reply through the wearable extender.
        return Notification.WearableExtender(n).actions
            .filterNotNull()
            .firstOrNull { !it.remoteInputs.isNullOrEmpty() }
    }

    private fun isReplyLike(action: Notification.Action): Boolean {
        if (android.os.Build.VERSION.SDK_INT >= 28 &&
            action.semanticAction == Notification.Action.SEMANTIC_ACTION_REPLY
        ) return true
        val title = action.title?.toString()?.lowercase() ?: return false
        // Covers "Reply", "Responder", "Répondre"…
        return title.contains("reply") || title.contains("respon") || title.contains("répond")
    }

    private fun sendReply(action: Notification.Action, text: String): Boolean {
        val remoteInputs = action.remoteInputs?.takeIf { it.isNotEmpty() } ?: return false
        // actionIntent is a platform field some OEM notifications leave null.
        val pendingIntent = action.actionIntent ?: return false
        val intent = Intent()
        val results = Bundle()
        remoteInputs.forEach { ri -> ri?.resultKey?.let { results.putCharSequence(it, text) } }
        RemoteInput.addResultsToIntent(remoteInputs, intent, results)
        return try {
            pendingIntent.send(this, 0, intent)
            true
        } catch (e: PendingIntent.CanceledException) {
            Log.w(TAG, "reply intent canceled", e)
            false
        } catch (t: Throwable) {
            // Some OEM PendingIntents throw beyond CanceledException (dead
            // process, revoked permissions); a failed send is a logged "no",
            // never a crash.
            Log.w(TAG, "reply intent send failed", t)
            false
        }
    }

    // ----------------------------------------------------------------- logging

    private fun log(app: SupportedApp, msg: ParsedMessage, sent: Boolean, detail: String) {
        ReplyLog.add(
            applicationContext,
            ReplyEvent(
                timestamp = System.currentTimeMillis(),
                appPackage = app.packageName,
                appName = app.displayName,
                sender = msg.sender,
                incomingText = msg.text,
                replySent = sent,
                detail = detail
            )
        )
    }

    companion object {
        /**
         * What the core asked us to send, or null for "nothing". org.json's
         * optString turns a JSON null into the string "null"; that must never
         * reach a customer.
         */
        internal fun replyTextOf(resp: org.json.JSONObject?): String? {
            if (resp == null || resp.isNull("agentResponse")) return null
            return resp.optString("agentResponse").takeIf { it.isNotBlank() }
        }

        private const val TAG = "AgenteListener"
        private val PLACEHOLDERS = listOf(
            Regex("^(\\d+ )?new messages?( from \\d+ (chats?|conversations?))?\\.?$"),
            Regex("^checking for new messages\\.?(\\.\\.)?$"),
            Regex("^\\d+ (messages|chats)( from \\d+ (chats?|conversations?))?\\.?$"),
        )
        private const val IDENTITY_WINDOW_MS = 10 * 60_000L
        /** Longest cooldown Settings allows (24 h) — prune horizon for lastReplied. */
        private const val MAX_COOLDOWN_MS = 25 * 60 * 60_000L
        private const val CREDITS_ALERT_INTERVAL_MS = 6 * 60 * 60_000L
        /** One holding line per customer conversation per this window. */
        private const val LIMIT_HOLDING_COOLDOWN_MS = 60 * 60_000L
        @Volatile private var lastCreditsAlert = 0L
    }
}
