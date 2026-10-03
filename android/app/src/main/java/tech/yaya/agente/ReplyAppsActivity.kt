package tech.yaya.agente

import android.content.Intent
import android.os.Bundle
import android.view.View
import android.widget.LinearLayout
import android.widget.TextView
import androidx.appcompat.app.AppCompatActivity
import com.google.android.material.button.MaterialButton

/**
 * Step 2 of 2 after the interview: of the apps the agent reads (step 1,
 * [ReadAppsActivity]), which ones it answers on — one switch per app, the
 * reply going out through the notification's own inline reply.
 *
 * Only apps a reply can go out on are listed: the chat apps agento knows
 * ([SupportedApps]) and the ones this phone learned ([ProfileStore]), whose
 * switch unlocks once they have read cleanly enough. A chat app the agent
 * does not read shows dimmed, pointing back to step 1. Wallets, banks and
 * the other apps read in step 1 carry no reply: the agent only reads them.
 *
 * WhatsApp Business answers by default; plain WhatsApp does when there is
 * no Business app, because since 1.21 the account *is* the WhatsApp number
 * the owner registered with. Reachable again from Settings ([EXTRA_EDIT]).
 */
class ReplyAppsActivity : AppCompatActivity() {

    companion object {
        const val EXTRA_EDIT = "edit"
    }

    private lateinit var list: LinearLayout
    private var edit = false

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(R.layout.activity_reply_apps)
        edit = intent.getBooleanExtra(EXTRA_EDIT, false)
        list = findViewById(R.id.reply_list)
        findViewById<TextView>(R.id.reply_step).visibility = if (edit) View.GONE else View.VISIBLE
        val cta = findViewById<MaterialButton>(R.id.reply_cta)
        cta.setText(if (edit) R.string.apps_save else R.string.apps_done)
        cta.setOnClickListener { finishSetup() }
        if (!Prefs.appsSetupDone(this)) applyChatDefaults()
    }

    override fun onResume() {
        super.onResume()
        // The read switches may have changed since (Settings, or back to step 1).
        buildRows()
    }

    /** First run only: the business number's own app answers by default. */
    private fun applyChatDefaults() {
        val w4b = AppToggles.isInstalled(this, "com.whatsapp.w4b")
        val wa = AppToggles.isInstalled(this, "com.whatsapp")
        if (!w4b && wa) Prefs.setAppEnabled(this, "com.whatsapp", true)
    }

    private fun buildRows() {
        list.removeAllViews()
        val (chat, missing) = SupportedApps.ALL.partition { AppToggles.isInstalled(this, it.packageName) }
        val learned = ProfileStore.all(this).filter { AppToggles.isInstalled(this, it.packageName) }
        var answerable = 0
        // Apps the agent reads first: those are the ones with a live switch.
        chat.sortedByDescending { Prefs.canRead(this, it.packageName) }.forEach { app ->
            val read = Prefs.canRead(this, app.packageName)
            if (read) answerable++
            AppToggles.addRow(
                this, list, app.packageName, app.displayName, available = read,
                subLabel = if (read) null else getString(R.string.reply_not_read),
                checked = Prefs.isAppEnabled(this, app.packageName),
            ) { on -> Prefs.setAppEnabled(this, app.packageName, on) }
        }
        // Apps this phone taught itself: the switch is offered once the
        // profile earned it; flipping it on after a pause gives the profile
        // a clean window.
        learned.forEach { p ->
            val read = Prefs.canRead(this, p.packageName)
            if (read) answerable++
            val sub = when {
                !read -> getString(R.string.reply_not_read)
                p.paused -> getString(R.string.settings_app_learned_paused)
                p.eligible -> getString(R.string.settings_app_learned_ready)
                else -> getString(R.string.settings_app_learned_observing, p.parsedOk, NotificationProfile.GRADUATE_OK)
            }
            AppToggles.addRow(
                this, list, p.packageName, p.displayName, available = read, subLabel = sub,
                checked = Prefs.isAppEnabled(this, p.packageName),
                switchEnabled = read && (p.eligible || p.paused),
            ) { on ->
                if (on && p.paused) ProfileStore.resume(this, p.packageName)
                Prefs.setAppEnabled(this, p.packageName, on)
                if (on && p.paused) list.post { buildRows() }
            }
        }
        val notes = listOfNotNull(
            when {
                chat.isEmpty() && learned.isEmpty() -> getString(R.string.reply_none_installed)
                answerable == 0 -> getString(R.string.reply_none_read)
                else -> null
            },
            missing.takeIf { it.isNotEmpty() && chat.isNotEmpty() }
                ?.let { m -> getString(R.string.reply_more, m.joinToString(", ") { it.displayName }) },
        )
        findViewById<TextView>(R.id.reply_note).apply {
            text = notes.joinToString("\n\n")
            visibility = if (notes.isEmpty()) View.GONE else View.VISIBLE
        }
    }

    private fun finishSetup() {
        Prefs.setAppsSetupDone(this)
        if (edit) { finish(); return }
        startActivity(Intent(this, DashboardActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TASK))
        overridePendingTransition(android.R.anim.fade_in, android.R.anim.fade_out)
        finish()
    }
}
