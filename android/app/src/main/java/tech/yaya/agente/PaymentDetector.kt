package tech.yaya.agente

import android.app.Notification
import android.content.Context
import android.content.pm.ApplicationInfo
import android.os.Build
import android.service.notification.NotificationListenerService
import android.service.notification.StatusBarNotification
import org.json.JSONArray
import org.json.JSONObject

/**
 * Decides which notifications the local agent gets to read. No list of
 * wallets, no patterns: every app that is not a chat app we reply to is
 * forwarded once, raw, and the agent's verdict comes back as "keep sending"
 * or "mute for a while" ([Prefs.muteSource]). Apps the network already
 * knows bring money in this country ([Prefs.learnedPaymentSources]) are
 * never muted. Support for a new wallet is nothing more than one phone
 * seeing it first.
 */
object PaymentDetector {

    /** The raw notification, ready for the agent. */
    data class Hit(val label: String, val packageName: String, val title: String, val text: String, val envelope: JSONObject)

    /**
     * Apps whose notification text is written by whoever wrote to the owner,
     * not by the app itself: the phone's SMS app (sender ids are spoofable and
     * cheap in our market) and the mail clients. "Yape: recibiste S/ 200 de
     * JUAN PEREZ" costs an attacker one SMS or one email, and the agent has no
     * way to tell it from a real notice. A wallet posts its own notification;
     * it never asks the SMS or mail app to post one for it.
     *
     * Checked before the owner's switches: no setting opens this door.
     */
    private val NEVER_MONEY = setOf(
        "com.google.android.gm",
        "com.microsoft.office.outlook",
        "com.yahoo.mobile.client.android.mail",
        "com.samsung.android.email.provider",
        "com.android.email",
        "ch.protonmail.android",
        "me.bluemail.mail",
        "com.fsck.k9",
    )

    private fun isNeverMoney(ctx: Context, pkg: String): Boolean {
        if (pkg in NEVER_MONEY) return true
        val sms = runCatching { android.provider.Telephony.Sms.getDefaultSmsPackage(ctx) }.getOrNull()
        return sms != null && pkg == sms
    }

    fun inspect(ctx: Context, listener: NotificationListenerService, sbn: StatusBarNotification): Hit? {
        val pkg = sbn.packageName ?: return null
        if (pkg == ctx.packageName) return null
        if (SupportedApps.isSupported(pkg)) return null
        if (isNeverMoney(ctx, pkg)) return null
        val n = sbn.notification ?: return null
        if (n.flags and Notification.FLAG_GROUP_SUMMARY != 0) return null
        if (n.flags and Notification.FLAG_ONGOING_EVENT != 0) return null
        val extras = n.extras ?: return null
        val template = extras.getString(Notification.EXTRA_TEMPLATE) ?: ""
        // Conversations are handled by the chat path; a wallet never uses MessagingStyle.
        if (template.endsWith("MessagingStyle")) return null
        val title = extras.getCharSequence(Notification.EXTRA_TITLE)?.toString() ?: ""
        val text = extras.getCharSequence(Notification.EXTRA_TEXT)?.toString() ?: ""
        val bigText = extras.getCharSequence(Notification.EXTRA_BIG_TEXT)?.toString()
        val subText = extras.getCharSequence(Notification.EXTRA_SUB_TEXT)?.toString()
        val infoText = extras.getCharSequence(Notification.EXTRA_INFO_TEXT)?.toString()
        val summaryText = extras.getCharSequence(Notification.EXTRA_SUMMARY_TEXT)?.toString()
        val lines = extras.getCharSequenceArray(Notification.EXTRA_TEXT_LINES)?.map { it.toString() } ?: emptyList()
        val body = text.ifBlank { bigText ?: lines.joinToString(" ") }
        if (title.isBlank() && body.isBlank()) return null
        // The owner's word first (the read screen): an app they did not let
        // the agent read is never forwarded. Then the agent's own verdict: an
        // app it already said carries no money stays quiet until the mute
        // lapses.
        if (!Prefs.canRead(ctx, pkg)) return null
        val learned = Prefs.learnedPaymentSources(ctx).contains(pkg)
        if (!learned && Prefs.isSourceMuted(ctx, pkg)) return null

        val (channelId, channelName) = channelOf(listener, sbn)
        val appInfo = runCatching { ctx.packageManager.getApplicationInfo(pkg, 0) }.getOrNull()
        val label = appInfo?.let { ctx.packageManager.getApplicationLabel(it).toString() } ?: pkg
        val envelope = JSONObject()
            .put("package", pkg)
            .put("appLabel", label)
            .put("installer", installerOf(ctx, pkg))
            .put("systemApp", appInfo?.let { it.flags and ApplicationInfo.FLAG_SYSTEM != 0 } ?: false)
            .put("channelId", channelId)
            .put("channelName", channelName)
            .put("category", n.category)
            .put("template", template.substringAfterLast('$').substringAfterLast('.'))
            .put("title", title)
            .put("text", text)
            .put("subText", subText)
            .put("infoText", infoText)
            .put("summaryText", summaryText)
            .put("bigText", bigText)
            .put("textLines", JSONArray(lines))
            .put("postTime", sbn.postTime)
        return Hit(label, pkg, title, body, envelope)
    }

    private fun channelOf(listener: NotificationListenerService, sbn: StatusBarNotification): Pair<String?, String?> {
        val id = sbn.notification.channelId ?: return null to null
        val name = runCatching {
            listener.getNotificationChannels(sbn.packageName, sbn.user).firstOrNull { it.id == id }?.name?.toString()
        }.getOrNull()
        return id to name
    }

    private fun installerOf(ctx: Context, pkg: String): String? = runCatching {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) ctx.packageManager.getInstallSourceInfo(pkg).installingPackageName
        else @Suppress("DEPRECATION") ctx.packageManager.getInstallerPackageName(pkg)
    }.getOrNull()
}
