package tech.yaya.agente

import android.annotation.SuppressLint
import android.content.ActivityNotFoundException
import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.graphics.drawable.Drawable
import android.net.Uri
import android.os.Bundle
import android.os.PowerManager
import android.provider.Settings
import android.text.format.DateUtils
import android.view.Gravity
import android.view.LayoutInflater
import android.view.View
import android.view.ViewGroup
import android.widget.EditText
import android.widget.ImageView
import android.widget.TextView
import android.widget.Toast
import androidx.appcompat.app.AppCompatActivity
import androidx.core.content.ContextCompat
import androidx.recyclerview.widget.LinearLayoutManager
import androidx.recyclerview.widget.RecyclerView
import com.google.android.material.button.MaterialButton
import com.google.android.material.dialog.MaterialAlertDialogBuilder
import com.google.android.material.materialswitch.MaterialSwitch

class MainActivity : AppCompatActivity() {

    private lateinit var permissionBanner: View
    private lateinit var batteryCard: View
    private lateinit var masterSwitch: MaterialSwitch
    private lateinit var groupSwitch: MaterialSwitch
    private lateinit var clearSwitch: MaterialSwitch
    private lateinit var replyPreview: TextView
    private lateinit var cooldownPreview: TextView
    private lateinit var readAppsSummary: TextView
    private lateinit var replyAppsSummary: TextView
    private lateinit var logAdapter: LogAdapter

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(R.layout.activity_main)

        // This screen is Settings; WelcomeActivity owns first-run routing.
        if (Prefs.serverConfigured(this)) {
            findViewById<TextView>(R.id.main_title).text = getString(R.string.settings_title)
            findViewById<TextView>(R.id.main_subtitle).visibility = View.GONE
        }
        // Server URL is a dev tool: reveal with a long-press on the section header.
        findViewById<TextView>(R.id.server_header).setOnLongClickListener {
            val b = findViewById<MaterialButton>(R.id.server_config_button)
            b.visibility = if (b.visibility == View.VISIBLE) View.GONE else View.VISIBLE
            true
        }

        permissionBanner = findViewById(R.id.permission_banner)
        batteryCard = findViewById(R.id.battery_card)
        masterSwitch = findViewById(R.id.master_switch)
        groupSwitch = findViewById(R.id.group_switch)
        clearSwitch = findViewById(R.id.clear_switch)
        replyPreview = findViewById(R.id.reply_preview)
        cooldownPreview = findViewById(R.id.cooldown_preview)
        readAppsSummary = findViewById(R.id.read_apps_summary)
        replyAppsSummary = findViewById(R.id.reply_apps_summary)

        findViewById<MaterialButton>(R.id.grant_button).setOnClickListener {
            startActivity(Intent(Settings.ACTION_NOTIFICATION_LISTENER_SETTINGS))
        }

        findViewById<MaterialButton>(R.id.battery_allow_button).setOnClickListener {
            requestBatteryExemption()
        }
        findViewById<MaterialButton>(R.id.battery_dismiss_button).setOnClickListener {
            uiPrefs().edit().putBoolean(KEY_BATTERY_CARD_DISMISSED, true).apply()
            batteryCard.visibility = View.GONE
        }

        masterSwitch.setOnCheckedChangeListener { _, on ->
            if (on && !hasNotificationAccess()) {
                masterSwitch.isChecked = false
                promptForAccess()
            } else {
                Prefs.setEnabled(this, on)
            }
        }

        groupSwitch.setOnCheckedChangeListener { _, on -> Prefs.setReplyToGroups(this, on) }
        clearSwitch.setOnCheckedChangeListener { _, on -> Prefs.setClearAfterReply(this, on) }
        val shareSwitch = findViewById<MaterialSwitch>(R.id.share_switch)
        ServerClient.IO_EXECUTOR.execute {
            val a = ServerClient.account(this)
            runOnUiThread { shareSwitch.isChecked = a?.optBoolean("shareTraining") == true }
        }
        shareSwitch.setOnCheckedChangeListener { _, on -> ServerClient.IO_EXECUTOR.execute { ServerClient.accountShare(this, on) } }

        // yaya mesh: the post-quantum p2p VPN between agents. One consent
        // dialog (VpnService), then the core's config drives the tunnel.
        val meshSwitch = findViewById<com.google.android.material.materialswitch.MaterialSwitch>(R.id.mesh_switch)
        meshSwitch.isChecked = MeshTunnel.wanted(this)
        meshSwitch.setOnCheckedChangeListener { _, on ->
            if (on) {
                val consent = MeshTunnel.prepare(this)
                if (consent != null) { meshVpnConsent.launch(consent) } else { MeshTunnel.enable(this, true); refreshMeshStatus() }
            } else {
                MeshTunnel.enable(this, false); refreshMeshStatus()
            }
        }
        refreshMeshStatus()

        // Bring-your-own-model is a developer setting: every business API
        // goes through yaya.tech with the agent identity. Debug builds only.
        findViewById<MaterialButton>(R.id.server_config_button).apply {
            visibility = if (BuildConfig.DEBUG) View.VISIBLE else View.GONE
            setOnClickListener { editServerUrl() }
        }
        findViewById<MaterialButton>(R.id.payout_button).setOnClickListener {
            startActivity(Intent(this, PayoutActivity::class.java))
        }
        findViewById<MaterialButton>(R.id.support_button).setOnClickListener { Support.open(this) }
        findViewById<MaterialButton>(R.id.audit_button).setOnClickListener { startActivity(Intent(this, AuditActivity::class.java)) }
        findViewById<MaterialButton>(R.id.privacy_button).setOnClickListener { openUrl("https://agente.ceo/privacidad.html") }
        findViewById<MaterialButton>(R.id.terms_button).setOnClickListener { openUrl("https://agente.ceo/terminos.html") }
        // D17: the owner's data into the OS. A switch asks for its permission;
        // the flag is only set once the permission is really granted.
        findViewById<MaterialSwitch>(R.id.sync_contacts_switch).setOnCheckedChangeListener { btn, on ->
            if (!btn.isPressed) return@setOnCheckedChangeListener
            if (!on) { Prefs.setSyncContacts(this, false); return@setOnCheckedChangeListener }
            if (OsSync.hasContacts(this)) { Prefs.setSyncContacts(this, true); OsSync.syncAll(this) }
            else requestPermissions(OsSync.CONTACT_PERMS, OsSync.RC_CONTACTS)
        }
        findViewById<MaterialSwitch>(R.id.sync_calendar_switch).setOnCheckedChangeListener { btn, on ->
            if (!btn.isPressed) return@setOnCheckedChangeListener
            if (!on) { Prefs.setSyncCalendar(this, false); return@setOnCheckedChangeListener }
            if (OsSync.hasCalendar(this)) { Prefs.setSyncCalendar(this, true); OsSync.syncAll(this) }
            else requestPermissions(OsSync.CALENDAR_PERMS, OsSync.RC_CALENDAR)
        }
        // Keep the support line fresh: the server can switch it at any time.
        ServerClient.IO_EXECUTOR.execute { Prefs.rememberSupport(this, ServerClient.credits(this)) }
        findViewById<MaterialButton>(R.id.onboarding_button).setOnClickListener {
            startActivity(Intent(this, OnboardingActivity::class.java))
        }
        findViewById<MaterialButton>(R.id.credits_button).setOnClickListener {
            startActivity(Intent(this, CreditsActivity::class.java))
        }

        findViewById<View>(R.id.reply_row).setOnClickListener { editReplyText() }
        findViewById<View>(R.id.cooldown_row).setOnClickListener { editCooldown() }
        findViewById<View>(R.id.language_row).setOnClickListener { pickLanguage() }
        findViewById<View>(R.id.read_apps_row).setOnClickListener {
            startActivity(Intent(this, ReadAppsActivity::class.java).putExtra(ReadAppsActivity.EXTRA_EDIT, true))
        }
        findViewById<View>(R.id.reply_apps_row).setOnClickListener {
            startActivity(Intent(this, ReplyAppsActivity::class.java).putExtra(ReplyAppsActivity.EXTRA_EDIT, true))
        }
        findViewById<MaterialButton>(R.id.clear_log_button).setOnClickListener {
            ReplyLog.clear(this)
        }

        val logList = findViewById<RecyclerView>(R.id.log_list)
        logList.layoutManager = LinearLayoutManager(this)
        logAdapter = LogAdapter()
        logList.adapter = logAdapter
    }

    override fun onRequestPermissionsResult(requestCode: Int, permissions: Array<out String>, grantResults: IntArray) {
        super.onRequestPermissionsResult(requestCode, permissions, grantResults)
        val granted = grantResults.isNotEmpty() && grantResults.all { it == android.content.pm.PackageManager.PERMISSION_GRANTED }
        when (requestCode) {
            OsSync.RC_CONTACTS -> Prefs.setSyncContacts(this, granted)
            OsSync.RC_CALENDAR -> Prefs.setSyncCalendar(this, granted)
            else -> return
        }
        if (granted) OsSync.syncAll(this) else android.widget.Toast.makeText(this, R.string.ossync_permission_denied, android.widget.Toast.LENGTH_LONG).show()
        refreshOsSync()
    }

    private fun refreshOsSync() {
        findViewById<MaterialSwitch>(R.id.sync_contacts_switch).isChecked = Prefs.syncContacts(this) && OsSync.hasContacts(this)
        findViewById<MaterialSwitch>(R.id.sync_calendar_switch).isChecked = Prefs.syncCalendar(this) && OsSync.hasCalendar(this)
    }

    override fun onResume() {
        refreshOsSync()
        super.onResume()
        refresh()
        ReplyLog.listener = { runOnUiThread { logAdapter.reload() } }
    }

    override fun onPause() {
        super.onPause()
        ReplyLog.listener = null
    }

    private fun refresh() {
        ServerClient.IO_EXECUTOR.execute { MeshTunnel.sync(this) }
        refreshAppSummaries()
        val granted = hasNotificationAccess()
        permissionBanner.visibility = if (granted) View.GONE else View.VISIBLE
        // Access without battery exemption is the classic silent-death setup on
        // Xiaomi/Huawei — nudge until exempted or explicitly dismissed.
        val needsBatteryNudge = granted && !isBatteryExempt() &&
            !uiPrefs().getBoolean(KEY_BATTERY_CARD_DISMISSED, false)
        batteryCard.visibility = if (needsBatteryNudge) View.VISIBLE else View.GONE
        masterSwitch.isChecked = granted && Prefs.isEnabled(this)
        groupSwitch.isChecked = Prefs.replyToGroups(this)
        clearSwitch.isChecked = Prefs.clearAfterReply(this)
        replyPreview.text = Prefs.replyText(this)
        cooldownPreview.text = getString(R.string.cooldown_value, Prefs.cooldownMinutes(this))
        findViewById<TextView>(R.id.language_preview).text = AppLanguage.currentLabel(this)
        findViewById<TextView>(R.id.server_status).text =
            if (Prefs.serverConfigured(this)) getString(R.string.server_status_connected)
            else getString(R.string.server_status_off)
        findViewById<MaterialButton>(R.id.onboarding_button).text =
            if (Prefs.serverConfigured(this)) getString(R.string.server_setup_chat)
            else getString(R.string.server_setup_chat_new)
        logAdapter.reload()
        findViewById<View>(R.id.log_empty).visibility =
            if (logAdapter.itemCount == 0) View.VISIBLE else View.GONE
    }

    private fun hasNotificationAccess(): Boolean {
        val cn = ComponentName(this, AgenteNotificationListener::class.java)
        val enabled = Settings.Secure.getString(contentResolver, "enabled_notification_listeners")
        return enabled?.split(":")?.any {
            ComponentName.unflattenFromString(it) == cn
        } == true
    }

    // ------------------------------------------------------- battery exemption

    private fun isBatteryExempt(): Boolean =
        (getSystemService(POWER_SERVICE) as PowerManager)
            .isIgnoringBatteryOptimizations(packageName)

    @SuppressLint("BatteryLife")
    private fun requestBatteryExemption() {
        try {
            startActivity(
                Intent(
                    Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS,
                    Uri.parse("package:$packageName")
                )
            )
        } catch (_: ActivityNotFoundException) {
            // Some OEM builds strip the direct dialog — fall back to the list.
            try {
                startActivity(Intent(Settings.ACTION_IGNORE_BATTERY_OPTIMIZATION_SETTINGS))
            } catch (_: ActivityNotFoundException) {
            }
        }
    }

    private fun uiPrefs() = getSharedPreferences(UI_PREFS, Context.MODE_PRIVATE)

    // ----------------------------------------------------------------- dialogs

    private fun promptForAccess() {
        MaterialAlertDialogBuilder(this)
            .setTitle(R.string.permission_title)
            .setMessage(R.string.permission_explainer)
            .setPositiveButton(R.string.permission_open_settings) { _, _ ->
                startActivity(Intent(Settings.ACTION_NOTIFICATION_LISTENER_SETTINGS))
            }
            .setNegativeButton(android.R.string.cancel, null)
            .show()
    }

    private fun openUrl(url: String) {
        try { startActivity(Intent(Intent.ACTION_VIEW, Uri.parse(url))) }
        catch (e: ActivityNotFoundException) { Toast.makeText(this, url, Toast.LENGTH_LONG).show() }
    }

    private val meshVpnConsent = registerForActivityResult(androidx.activity.result.contract.ActivityResultContracts.StartActivityForResult()) { r ->
        if (r.resultCode == RESULT_OK) { MeshTunnel.enable(this, true) } else {
            findViewById<com.google.android.material.materialswitch.MaterialSwitch>(R.id.mesh_switch).isChecked = false
            findViewById<TextView>(R.id.mesh_status).text = getString(R.string.settings_mesh_denied)
        }
        refreshMeshStatus()
    }

    private fun refreshMeshStatus() {
        ServerClient.IO_EXECUTOR.execute {
            val s = if (MeshTunnel.wanted(this)) ServerClient.meshStatus(this) else null
            runOnUiThread {
                findViewById<TextView>(R.id.mesh_status).text = when {
                    s == null -> getString(R.string.settings_mesh_hint)
                    else -> getString(R.string.settings_mesh_on, s.optString("ip", "…"),
                        (0 until s.optJSONArray("peers")?.length().let { it ?: 0 }).count { s.optJSONArray("peers")?.optJSONObject(it)?.optString("status") == "linked" })
                }
            }
        }
    }

    /**
     * The turn the model sees carries the customer's words, so this endpoint
     * needs TLS — and the core is a native HTTP client, so
     * `network_security_config.xml` never had a say in it. Plain HTTP is
     * allowed only where it cannot leave the owner's own network, which is
     * the "even one on your LAN" case the explainer promises.
     */
    private fun isAllowedEngineUrl(u: String): Boolean {
        if (u.startsWith("https://")) return true
        if (!u.startsWith("http://")) return false
        val host = runCatching { Uri.parse(u).host }.getOrNull().orEmpty().lowercase()
        return host == "localhost" || host.endsWith(".local") ||
            host.startsWith("127.") || host.startsWith("10.") || host.startsWith("192.168.") ||
            Regex("^172\\.(1[6-9]|2[0-9]|3[01])\\.").containsMatchIn(host)
    }

    /** AI engine: blank = yaya.tech (free). Owners who want total control
     *  enter any OpenAI-compatible endpoint and their own key. Takes effect
     *  after the app restarts (the core reads its config at boot). */
    private fun editServerUrl() {
        val pad = (16 * resources.displayMetrics.density).toInt()
        val url = EditText(this).apply {
            hint = getString(R.string.ai_engine_url_hint)
            setText(Prefs.llmBaseUrl(this@MainActivity))
        }
        val key = EditText(this).apply {
            hint = getString(R.string.ai_engine_key_hint)
            setText(Prefs.llmApiKey(this@MainActivity))
            inputType = android.text.InputType.TYPE_CLASS_TEXT or
                android.text.InputType.TYPE_TEXT_VARIATION_PASSWORD
        }
        val model = EditText(this).apply {
            hint = getString(R.string.ai_engine_model_hint)
            setText(Prefs.llmModel(this@MainActivity))
        }
        val box = android.widget.LinearLayout(this).apply {
            orientation = android.widget.LinearLayout.VERTICAL
            setPadding(pad, pad / 2, pad, 0)
            addView(url); addView(key); addView(model)
        }
        MaterialAlertDialogBuilder(this)
            .setTitle(R.string.ai_engine_title)
            .setMessage(R.string.ai_engine_explainer)
            .setView(box)
            .setPositiveButton(android.R.string.ok) { _, _ ->
                val u = url.text.toString().trim()
                if (u.isNotEmpty() && !isAllowedEngineUrl(u)) {
                    Toast.makeText(this, R.string.server_url_must_be_https, Toast.LENGTH_LONG).show()
                    return@setPositiveButton
                }
                Prefs.setLlm(this, u, key.text.toString(), model.text.toString())
                Toast.makeText(this, R.string.ai_engine_saved, Toast.LENGTH_LONG).show()
                refresh()
            }
            .setNeutralButton(R.string.ai_engine_reset) { _, _ ->
                Prefs.setLlm(this, "", "", "")
                Toast.makeText(this, R.string.ai_engine_saved, Toast.LENGTH_LONG).show()
                refresh()
            }
            .setNegativeButton(android.R.string.cancel, null)
            .show()
    }

    private fun editReplyText() {
        val input = EditText(this).apply {
            setText(Prefs.replyText(this@MainActivity))
            minLines = 3
        }
        MaterialAlertDialogBuilder(this)
            .setTitle(R.string.reply_message)
            .setView(input)
            .setPositiveButton(android.R.string.ok) { _, _ ->
                val t = input.text.toString().trim()
                if (t.isNotEmpty()) Prefs.setReplyText(this, t)
                refresh()
            }
            .setNegativeButton(android.R.string.cancel, null)
            .show()
    }

    /** Ajustes → Idioma: follow the phone, or one of the three the app ships. */
    private fun pickLanguage() {
        val labels = AppLanguage.OPTIONS.map { getString(it.second) }.toTypedArray()
        val current = AppLanguage.OPTIONS.indexOfFirst { it.first == AppLanguage.currentTag() }.coerceAtLeast(0)
        MaterialAlertDialogBuilder(this)
            .setTitle(R.string.settings_language)
            .setSingleChoiceItems(labels, current) { d, which ->
                AppLanguage.apply(AppLanguage.OPTIONS[which].first)
                d.dismiss()
            }
            .setNegativeButton(android.R.string.cancel, null)
            .show()
    }

    private fun editCooldown() {
        val input = EditText(this).apply {
            setText(Prefs.cooldownMinutes(this@MainActivity).toString())
            inputType = android.text.InputType.TYPE_CLASS_NUMBER
        }
        MaterialAlertDialogBuilder(this)
            .setTitle(R.string.cooldown_title)
            .setView(input)
            .setPositiveButton(android.R.string.ok) { _, _ ->
                input.text.toString().toIntOrNull()?.let {
                    Prefs.setCooldownMinutes(this, it.coerceIn(0, 24 * 60))
                }
                refresh()
            }
            .setNegativeButton(android.R.string.cancel, null)
            .show()
    }

    // -------------------------------------------------------------- apps

    /** One line each under Settings → Apps conectadas: what is read, where it answers. */
    private fun refreshAppSummaries() {
        val answers = SupportedApps.ALL.map { it.packageName to it.displayName }
            .filter { (pkg, _) -> Prefs.isAppEnabled(this, pkg) } +
            ProfileStore.all(this).filter { ProfileStore.isLive(this, it) }.map { it.packageName to it.displayName }
        val live = answers.filter { (pkg, _) -> AppToggles.isInstalled(this, pkg) && Prefs.canRead(this, pkg) }
        replyAppsSummary.text = if (live.isEmpty()) getString(R.string.settings_reply_apps_none)
                                else live.joinToString(", ") { it.second }
        val ctx = applicationContext
        ServerClient.IO_EXECUTOR.execute {
            val apps = PhoneApps.list(ctx)
            val read = apps.count { Prefs.canRead(ctx, it.packageName) }
            runOnUiThread {
                if (!isDestroyed) readAppsSummary.text = getString(R.string.read_count, read, apps.size)
            }
        }
    }

    private val iconCache = HashMap<String, Drawable?>()

    private fun appIcon(pkg: String): Drawable? = iconCache.getOrPut(pkg) { AppToggles.appIcon(this, pkg) }

    // ------------------------------------------------------------ log adapter

    private inner class LogAdapter : RecyclerView.Adapter<LogAdapter.Holder>() {
        private var events: List<ReplyEvent> = emptyList()

        fun reload() {
            events = ReplyLog.load(this@MainActivity)
            notifyDataSetChanged()
            findViewById<View>(R.id.log_empty)?.visibility =
                if (events.isEmpty()) View.VISIBLE else View.GONE
        }

        inner class Holder(v: View) : RecyclerView.ViewHolder(v) {
            val icon: ImageView = v.findViewById(R.id.log_app_icon)
            val line1: TextView = v.findViewById(R.id.log_line1)
            val time: TextView = v.findViewById(R.id.log_time)
            val line2: TextView = v.findViewById(R.id.log_line2)
            val line3: TextView = v.findViewById(R.id.log_line3)
        }

        override fun onCreateViewHolder(parent: ViewGroup, viewType: Int): Holder {
            val v = LayoutInflater.from(parent.context)
                .inflate(R.layout.item_log, parent, false)
            return Holder(v)
        }

        override fun getItemCount() = events.size

        override fun onBindViewHolder(holder: Holder, position: Int) {
            val e = events[position]
            holder.icon.setImageDrawable(appIcon(e.appPackage))
            holder.line1.text = "${e.appName} · ${e.sender}"
            holder.time.text = relativeTime(e.timestamp)
            holder.line2.text = e.incomingText

            val (status, color) = when {
                e.detail.startsWith("💰") ->
                    e.detail to R.color.agente_secondary
                e.replySent ->
                    getString(R.string.log_replied) to R.color.agente_primary
                e.detail == getString(R.string.log_send_failed) ->
                    e.detail to R.color.agente_error
                else ->
                    e.detail to R.color.agente_on_surface_muted
            }
            holder.line3.text = status
            holder.line3.setTextColor(ContextCompat.getColor(this@MainActivity, color))
        }

        private fun relativeTime(ts: Long): CharSequence {
            val now = System.currentTimeMillis()
            if (now - ts < DateUtils.MINUTE_IN_MILLIS) return getString(R.string.log_relative_now)
            return DateUtils.getRelativeTimeSpanString(
                ts, now, DateUtils.MINUTE_IN_MILLIS, DateUtils.FORMAT_ABBREV_RELATIVE
            )
        }
    }

    companion object {
        private const val UI_PREFS = "agente_settings_ui"
        private const val KEY_BATTERY_CARD_DISMISSED = "battery_card_dismissed"
    }
}
