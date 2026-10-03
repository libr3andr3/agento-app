package tech.yaya.agente

import android.os.Bundle
import android.view.View
import android.view.ViewGroup
import android.widget.TextView
import android.widget.Toast
import androidx.appcompat.app.AppCompatActivity
import androidx.recyclerview.widget.LinearLayoutManager
import androidx.recyclerview.widget.RecyclerView
import com.google.android.material.button.MaterialButton
import com.google.android.material.card.MaterialCardView
import com.google.android.material.chip.Chip
import com.google.android.material.dialog.MaterialAlertDialogBuilder
import org.json.JSONObject
import java.text.SimpleDateFormat
import java.util.Locale

/**
 * The audit chain, read from the phone (2026-08-30): every turn, tool call,
 * remote command, payment, status change and share the agent took part in —
 * hash-chained, signed by this installation's identity, anchored to the
 * gateway's clock. The header is the verdict of a full verification; each
 * row says whether an anchor already covers it.
 */
class AuditActivity : AppCompatActivity() {

    private val entries = ArrayList<JSONObject>()
    private lateinit var list: RecyclerView
    private lateinit var verdict: TextView
    private lateinit var verdictCard: MaterialCardView
    private var loading = false
    private var exhausted = false

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(R.layout.activity_audit)
        findViewById<View>(R.id.audit_back).setOnClickListener { finish() }
        verdict = findViewById(R.id.audit_verdict)
        verdictCard = findViewById(R.id.audit_verdict_card)
        list = findViewById(R.id.audit_list)
        list.layoutManager = LinearLayoutManager(this)
        list.adapter = Adapter()
        list.addOnScrollListener(object : RecyclerView.OnScrollListener() {
            override fun onScrolled(rv: RecyclerView, dx: Int, dy: Int) {
                val lm = rv.layoutManager as LinearLayoutManager
                if (!loading && !exhausted && lm.findLastVisibleItemPosition() >= entries.size - 8) loadMore()
            }
        })
        findViewById<MaterialButton>(R.id.audit_anchor).setOnClickListener { anchorNow() }
        verify()
        loadMore()
    }

    private fun verify() {
        ServerClient.IO_EXECUTOR.execute {
            val v = ServerClient.auditVerify(this)
            runOnUiThread { if (!isFinishing) bindVerdict(v) }
        }
    }

    private fun bindVerdict(v: JSONObject?) {
        if (v == null) {
            verdict.text = getString(R.string.audit_verify_offline)
            verdictCard.setCardBackgroundColor(getColor(R.color.agente_surface_variant))
            return
        }
        val ok = v.optBoolean("ok")
        val n = v.optInt("entries")
        val unanchored = v.optInt("unanchoredEntries")
        val anchor = v.optJSONObject("anchor")
        val when_ = anchor?.optString("anchoredAt")?.takeIf { it.isNotBlank() && it != "null" }?.let { shortTime(it) }
        verdict.text = when {
            !ok -> getString(R.string.audit_verify_broken, v.optJSONArray("problems")?.length() ?: 0)
            anchor == null -> getString(R.string.audit_verify_ok_unanchored, n)
            else -> getString(R.string.audit_verify_ok, n, when_ ?: "", unanchored)
        }
        verdictCard.setCardBackgroundColor(getColor(if (ok) R.color.agente_primary_container else R.color.agente_error_container))
        verdict.setTextColor(getColor(if (ok) R.color.agente_on_primary_container else R.color.agente_error))
        findViewById<TextView>(R.id.audit_agent).text = getString(R.string.audit_agent_id, v.optString("agent").take(22) + "…")
    }

    private fun anchorNow() {
        ServerClient.IO_EXECUTOR.execute {
            val r = ServerClient.auditAnchor(this)
            runOnUiThread {
                if (isFinishing) return@runOnUiThread
                Toast.makeText(this, if (r.code in 200..299) R.string.audit_anchored else R.string.audit_anchor_failed, Toast.LENGTH_LONG).show()
                if (r.code in 200..299) { verify(); entries.clear(); exhausted = false; list.adapter?.notifyDataSetChanged(); loadMore() }
            }
        }
    }

    private fun loadMore() {
        loading = true
        val before = entries.lastOrNull()?.optLong("seq")
        ServerClient.IO_EXECUTOR.execute {
            val page = ServerClient.audit(this, before)?.optJSONArray("entries")
            runOnUiThread {
                if (isFinishing) return@runOnUiThread
                loading = false
                if (page == null || page.length() == 0) { exhausted = true; findViewById<View>(R.id.audit_empty).visibility = if (entries.isEmpty()) View.VISIBLE else View.GONE; return@runOnUiThread }
                val start = entries.size
                for (i in 0 until page.length()) entries.add(page.getJSONObject(i))
                list.adapter?.notifyItemRangeInserted(start, page.length())
                findViewById<View>(R.id.audit_empty).visibility = View.GONE
            }
        }
    }

    private val iso = SimpleDateFormat("yyyy-MM-dd'T'HH:mm:ss", Locale.US).apply { timeZone = java.util.TimeZone.getTimeZone("UTC") }
    private fun shortTime(ts: String): String {
        val d = runCatching { iso.parse(ts.take(19)) }.getOrNull() ?: return ts
        return SimpleDateFormat("d MMM HH:mm:ss", Locale.getDefault()).format(d)
    }

    /** One line a human reads; the payload stays a tap away. */
    private fun summary(e: JSONObject): String {
        val p = e.optJSONObject("payload") ?: JSONObject()
        val subject = e.optString("subject")
        return when (e.optString("kind")) {
            "customer_turn" -> getString(R.string.audit_kind_customer_turn, e.optString("actor").substringAfter("customer:").substringAfterLast(':'), p.optString("reply").take(90))
            "owner_turn" -> getString(R.string.audit_kind_owner_turn, p.optString("reply").take(90))
            "tool_call" -> getString(R.string.audit_kind_tool_call, subject, p.optString("status").takeIf { it != "null" } ?: "")
            "owner_cmd" -> getString(R.string.audit_kind_owner_cmd, subject, e.optString("actor").substringAfter("owner:").take(18))
            "payment" -> getString(R.string.audit_kind_payment, p.optString("wallet"), p.optDouble("amount", 0.0).let { Prefs.money(this, it) })
            "status" -> getString(R.string.audit_kind_status, p.optString("what"), p.optString("set"))
            "share" -> getString(R.string.audit_kind_share, p.optInt("photos"))
            "restore" -> getString(R.string.audit_kind_restore, p.optInt("rows"))
            "boot" -> getString(R.string.audit_kind_boot, p.optString("version"))
            else -> e.optString("kind")
        }
    }

    private inner class Adapter : RecyclerView.Adapter<Adapter.VH>() {
        inner class VH(v: View) : RecyclerView.ViewHolder(v) {
            val time: TextView = v.findViewById(R.id.audit_time)
            val text: TextView = v.findViewById(R.id.audit_text)
            val kind: Chip = v.findViewById(R.id.audit_kind)
            val anchored: TextView = v.findViewById(R.id.audit_anchored)
        }
        override fun onCreateViewHolder(parent: ViewGroup, viewType: Int) = VH(layoutInflater.inflate(R.layout.item_audit, parent, false))
        override fun getItemCount() = entries.size
        override fun onBindViewHolder(h: VH, i: Int) {
            val e = entries[i]
            h.time.text = "#${e.optLong("seq")} · ${shortTime(e.optString("ts"))}"
            h.text.text = summary(e)
            h.kind.text = e.optString("kind").replace('_', ' ')
            h.anchored.text = getString(if (e.optBoolean("anchored")) R.string.audit_row_anchored else R.string.audit_row_pending)
            h.anchored.setTextColor(getColor(if (e.optBoolean("anchored")) R.color.agente_primary else R.color.agente_on_surface_muted))
            h.itemView.setOnClickListener {
                MaterialAlertDialogBuilder(this@AuditActivity)
                    .setTitle("#${e.optLong("seq")} · ${e.optString("kind")}")
                    .setMessage(getString(R.string.audit_detail, e.optString("ts"), e.optString("actor"), e.optString("hash"), e.optJSONObject("payload")?.toString(2) ?: ""))
                    .setPositiveButton(android.R.string.ok, null)
                    .show()
            }
        }
    }
}
