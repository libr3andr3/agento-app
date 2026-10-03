package tech.yaya.agente

import android.content.Intent
import android.os.Bundle
import android.text.Editable
import android.text.TextWatcher
import android.view.LayoutInflater
import android.view.View
import android.widget.LinearLayout
import android.widget.TextView
import androidx.appcompat.app.AppCompatActivity
import androidx.swiperefreshlayout.widget.SwipeRefreshLayout
import com.google.android.material.textfield.TextInputEditText
import com.google.android.material.textfield.TextInputLayout
import org.json.JSONArray
import org.json.JSONObject
import java.text.SimpleDateFormat
import java.util.Locale

/**
 * Two lists from one screen: Conversaciones (what the agent is handling
 * right now) and Clientes (the CRM). Both come from the core on the phone;
 * a row opens the conversation log.
 */
class CrmListActivity : AppCompatActivity() {

    companion object {
        const val EXTRA_MODE = "mode"
        const val MODE_CONVERSATIONS = "conversations"
        const val MODE_CONTACTS = "contacts"
    }

    private lateinit var list: LinearLayout
    private lateinit var swipe: SwipeRefreshLayout
    private var mode = MODE_CONVERSATIONS
    private var query = ""

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(R.layout.activity_crm_list)
        mode = intent.getStringExtra(EXTRA_MODE) ?: MODE_CONVERSATIONS
        list = findViewById(R.id.crm_list)
        swipe = findViewById(R.id.crm_swipe)
        findViewById<TextView>(R.id.crm_title).setText(if (mode == MODE_CONTACTS) R.string.crm_contacts_title else R.string.crm_conversations_title)
        if (mode == MODE_CONTACTS) {
            findViewById<TextView>(R.id.crm_export).apply {
                visibility = View.VISIBLE
                setOnClickListener {
                    com.google.android.material.dialog.MaterialAlertDialogBuilder(this@CrmListActivity)
                        .setTitle(R.string.crm_export)
                        .setItems(arrayOf(getString(R.string.crm_export_vcf), getString(R.string.crm_export_csv))) { _, which ->
                            ServerClient.IO_EXECUTOR.execute {
                                val rows = ServerClient.contacts(this@CrmListActivity, "")?.optJSONArray("contacts")
                                runOnUiThread {
                                    if (rows == null || rows.length() == 0) { android.widget.Toast.makeText(this@CrmListActivity, R.string.export_nothing, android.widget.Toast.LENGTH_SHORT).show(); return@runOnUiThread }
                                    if (which == 0) OsSync.shareFile(this@CrmListActivity, "clientes.vcf", "text/x-vcard", OsSync.vcard(rows))
                                    else OsSync.shareFile(this@CrmListActivity, "clientes.csv", "text/csv", OsSync.csv(rows))
                                }
                            }
                        }
                        .show()
                }
            }
            findViewById<TextInputLayout>(R.id.crm_search_til).visibility = View.VISIBLE
            findViewById<TextInputEditText>(R.id.crm_search).addTextChangedListener(object : TextWatcher {
                override fun beforeTextChanged(s: CharSequence?, a: Int, b: Int, c: Int) {}
                override fun onTextChanged(s: CharSequence?, a: Int, b: Int, c: Int) {}
                override fun afterTextChanged(s: Editable?) { query = s?.toString().orEmpty(); load() }
            })
        }
        swipe.setOnRefreshListener { load() }
        load()
    }

    override fun onResume() { super.onResume(); load() }

    private fun load() {
        swipe.isRefreshing = true
        ServerClient.IO_EXECUTOR.execute {
            val rows = if (mode == MODE_CONTACTS) ServerClient.contacts(this, query)?.optJSONArray("contacts")
                       else ServerClient.conversations(this)?.optJSONArray("conversations")
            runOnUiThread { swipe.isRefreshing = false; render(rows ?: JSONArray()) }
        }
    }

    private fun render(rows: JSONArray) {
        list.removeAllViews()
        if (rows.length() == 0) {
            list.addView(TextView(this).apply {
                setText(if (mode == MODE_CONTACTS) R.string.crm_contacts_empty else R.string.crm_conversations_empty)
                setTextAppearance(R.style.TextAppearance_Agente_Body)
                setTextColor(getColor(R.color.agente_on_surface_muted))
                setPadding(resources.getDimensionPixelSize(R.dimen.space_m), resources.getDimensionPixelSize(R.dimen.space_l), resources.getDimensionPixelSize(R.dimen.space_m), 0)
            })
            return
        }
        val inf = LayoutInflater.from(this)
        for (i in 0 until rows.length()) {
            val r = rows.getJSONObject(i)
            val v = inf.inflate(R.layout.item_crm_row, list, false)
            val contact = if (mode == MODE_CONTACTS) r else r.optJSONObject("contact") ?: JSONObject()
            val name = Crm.displayName(this, contact, r.optString("peer"))
            v.findViewById<TextView>(R.id.row_avatar).text = Crm.initials(name)
            v.findViewById<TextView>(R.id.row_name).text = name
            v.findViewById<TextView>(R.id.row_meta).text = listOf(Crm.metaLine(this, contact), Crm.planLabel(this, contact))
                .filter { it.isNotBlank() }.joinToString(" · ")
            val time = v.findViewById<TextView>(R.id.row_time)
            val preview = v.findViewById<TextView>(R.id.row_preview)
            if (mode == MODE_CONTACTS) {
                time.text = Crm.shortTime(contact.optString("lastSeen"))
                preview.text = getString(R.string.crm_messages_count, contact.optInt("messages"))
            } else {
                time.text = Crm.shortTime(r.optString("lastAt"))
                val who = if (r.optString("lastRole") == "assistant") "🤖 " else ""
                preview.text = who + r.optString("lastText")
            }
            val peer = if (mode == MODE_CONTACTS) contact.optString("peer").takeIf { it.isNotEmpty() && it != "null" && it != "owner" } else r.optString("peer")
            if (peer != null) v.setOnClickListener {
                startActivity(Intent(this, ConversationActivity::class.java).putExtra(ConversationActivity.EXTRA_PEER, peer))
            }
            list.addView(v)
        }
    }
}

/** Presentation helpers shared by the CRM screens and the dashboard. */
object Crm {
    /** D14: on a seller's phone the core marks contacts with their agente plan. */
    fun planLabel(ctx: android.content.Context, contact: JSONObject?): String = when (contact?.optString("plan").orEmpty()) {
        "pro" -> ctx.getString(R.string.crm_plan_pro)
        "max", "custom", "enterprise" -> ctx.getString(R.string.crm_plan_max)
        "trial" -> ctx.getString(R.string.crm_plan_trial)
        "free" -> ctx.getString(R.string.crm_plan_free)
        "lead" -> ctx.getString(R.string.crm_plan_lead)
        else -> ""
    }

    fun displayName(ctx: android.content.Context, contact: JSONObject?, peer: String): String {
        val n = contact?.optString("name").orEmpty()
        if (n.isNotBlank() && n != "null") return n
        val phone = contact?.optString("phone").orEmpty()
        if (phone.isNotBlank() && phone != "null") return "+$phone"
        if (peer.startsWith("agent:")) return ctx.getString(R.string.crm_source_network)
        val tail = peer.substringAfterLast(':')
        return tail.ifBlank { ctx.getString(R.string.crm_unknown_name) }
    }

    fun initials(name: String): String =
        name.split(' ').filter { it.isNotBlank() && it[0].isLetter() }.take(2).map { it[0].uppercaseChar() }.joinToString("").ifEmpty { "☺" }

    fun metaLine(ctx: android.content.Context, contact: JSONObject?): String {
        val parts = mutableListOf<String>()
        when (contact?.optString("kind")) {
            "owner" -> parts.add(ctx.getString(R.string.crm_kind_owner))
        }
        val src = contact?.optString("source").orEmpty()
        if (src == "network") parts.add(ctx.getString(R.string.crm_source_network)) else if (src.isNotBlank() && src != "null" && src != "owner") parts.add(src.replaceFirstChar { it.uppercase() })
        val phone = contact?.optString("phone").orEmpty(); if (phone.isNotBlank() && phone != "null" && contact?.optString("name").orEmpty().let { it.isNotBlank() && it != "null" }) parts.add("+$phone")
        val email = contact?.optString("email").orEmpty(); if (email.isNotBlank() && email != "null") parts.add(email)
        return parts.joinToString(" · ")
    }

    private val iso = SimpleDateFormat("yyyy-MM-dd'T'HH:mm:ss", Locale.US).apply { timeZone = java.util.TimeZone.getTimeZone("UTC") }
    fun shortTime(at: String): String {
        val d = runCatching { iso.parse(at.take(19)) }.getOrNull() ?: return ""
        val now = System.currentTimeMillis()
        val fmt = if (now - d.time < 24L * 3600 * 1000) SimpleDateFormat("HH:mm", Locale.getDefault()) else SimpleDateFormat("d MMM", Locale.getDefault())
        return fmt.format(d)
    }
}
