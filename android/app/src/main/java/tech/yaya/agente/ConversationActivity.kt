package tech.yaya.agente

import android.os.Bundle
import android.view.LayoutInflater
import android.view.View
import android.widget.LinearLayout
import android.widget.TextView
import android.widget.Toast
import androidx.appcompat.app.AppCompatActivity
import com.google.android.material.bottomsheet.BottomSheetDialog
import com.google.android.material.textfield.TextInputEditText
import org.json.JSONObject

/**
 * One customer's log: what they said, what the agent answered, and what
 * the agent did in between (checked availability, booked, verified a
 * payment…). The header is the contact; tap it to correct name/email/notes.
 */
class ConversationActivity : AppCompatActivity() {

    companion object { const val EXTRA_PEER = "peer" }

    private lateinit var list: LinearLayout
    private var peer = ""
    private var contact: JSONObject? = null

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(R.layout.activity_conversation)
        peer = intent.getStringExtra(EXTRA_PEER).orEmpty()
        list = findViewById(R.id.conv_list)
        findViewById<View>(R.id.conv_header).setOnClickListener { edit() }
        load()
    }

    private fun load() {
        ServerClient.IO_EXECUTOR.execute {
            val v = ServerClient.conversation(this, peer)
            runOnUiThread { if (v != null) render(v) }
        }
    }

    private fun render(v: JSONObject) {
        contact = v.optJSONObject("contact")
        val name = Crm.displayName(this, contact, peer)
        findViewById<TextView>(R.id.conv_avatar).text = Crm.initials(name)
        findViewById<TextView>(R.id.conv_name).text = name
        findViewById<TextView>(R.id.conv_meta).text = Crm.metaLine(this, contact)
        list.removeAllViews()
        val inf = LayoutInflater.from(this)
        val items = v.optJSONArray("items") ?: return
        for (i in 0 until items.length()) {
            val it = items.getJSONObject(i)
            when (it.optString("kind")) {
                "message" -> {
                    // The customer on the left, the business (its agent) on the right.
                    val mine = it.optString("role") == "assistant"
                    val row = inf.inflate(if (mine) R.layout.item_chat_owner else R.layout.item_chat_agent, list, false)
                    row.findViewById<TextView>(R.id.bubble_text).text = it.optString("text")
                    list.addView(row)
                }
                "action" -> {
                    val row = inf.inflate(R.layout.item_chat_system, list, false)
                    val ok = it.optBoolean("ok", true)
                    val label = it.optString("label") + it.optString("summary").takeIf { s -> s.isNotBlank() }?.let { s -> " · $s" }.orEmpty()
                    row.findViewById<TextView>(R.id.system_text).text =
                        getString(if (ok) R.string.crm_agent_action else R.string.crm_agent_action_failed, label)
                    list.addView(row)
                }
            }
        }
        findViewById<android.widget.ScrollView>(R.id.conv_scroll).post { findViewById<android.widget.ScrollView>(R.id.conv_scroll).fullScroll(View.FOCUS_DOWN) }
    }

    private fun edit() {
        val c = contact ?: return
        val dialog = BottomSheetDialog(this)
        val view = LayoutInflater.from(this).inflate(R.layout.sheet_edit_contact, null)
        dialog.setContentView(view)
        val nameIn = view.findViewById<TextInputEditText>(R.id.edit_name); nameIn.setText(c.optString("name").takeIf { it != "null" })
        val emailIn = view.findViewById<TextInputEditText>(R.id.edit_email); emailIn.setText(c.optString("email").takeIf { it != "null" })
        val notesIn = view.findViewById<TextInputEditText>(R.id.edit_notes); notesIn.setText(c.optString("notes").takeIf { it != "null" })
        view.findViewById<View>(R.id.edit_save).setOnClickListener {
            val body = JSONObject().put("name", nameIn.text.toString().trim()).put("email", emailIn.text.toString().trim()).put("notes", notesIn.text.toString().trim())
            ServerClient.IO_EXECUTOR.execute {
                val r = ServerClient.updateContact(this, c.optString("id"), body)
                runOnUiThread {
                    if (r != null) { Toast.makeText(this, R.string.crm_saved, Toast.LENGTH_SHORT).show(); dialog.dismiss(); load() }
                    else Toast.makeText(this, R.string.plan_error, Toast.LENGTH_SHORT).show()
                }
            }
        }
        dialog.show()
    }
}
