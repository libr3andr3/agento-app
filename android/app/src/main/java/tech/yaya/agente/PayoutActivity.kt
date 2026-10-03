package tech.yaya.agente

import android.content.Intent
import android.os.Bundle
import android.view.LayoutInflater
import android.view.View
import android.widget.ArrayAdapter
import android.widget.LinearLayout
import android.widget.TextView
import android.widget.Toast
import androidx.appcompat.app.AppCompatActivity
import com.google.android.material.button.MaterialButton
import com.google.android.material.materialswitch.MaterialSwitch
import com.google.android.material.progressindicator.LinearProgressIndicator
import com.google.android.material.textfield.MaterialAutoCompleteTextView
import com.google.android.material.textfield.TextInputEditText
import com.google.android.material.textfield.TextInputLayout
import org.json.JSONArray
import org.json.JSONObject

/**
 * Cobros — where customers send money. Step 4 of registration and a row
 * in Settings. The owner types the wallet's name and the destination in
 * their own words; the only help is a list of names other owners in the
 * country use. After saving, if the network knows which app a wallet
 * notifies through and it is not on this phone, the owner is told that
 * payments will not confirm here.
 */
class PayoutActivity : AppCompatActivity() {

    companion object {
        const val EXTRA_FROM_REGISTRATION = "from_registration"
    }

    private lateinit var rows: LinearLayout
    private lateinit var holder: TextInputEditText
    private lateinit var cashOnly: MaterialSwitch
    private lateinit var save: MaterialButton
    private lateinit var progress: LinearProgressIndicator
    private lateinit var error: TextView
    private lateinit var warning: View
    private var suggestions: List<JSONObject> = emptyList()
    private var fromRegistration = false

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(R.layout.activity_payout)
        fromRegistration = intent.getBooleanExtra(EXTRA_FROM_REGISTRATION, false)
        rows = findViewById(R.id.payout_rows)
        holder = findViewById(R.id.payout_holder)
        cashOnly = findViewById(R.id.payout_cash_only)
        save = findViewById(R.id.payout_save)
        progress = findViewById(R.id.payout_progress)
        error = findViewById(R.id.payout_error)
        warning = findViewById(R.id.payout_warning)
        findViewById<TextView>(R.id.payout_step).visibility = if (fromRegistration) View.VISIBLE else View.GONE
        findViewById<TextView>(R.id.payout_step).setText(R.string.payout_step)
        save.setText(if (fromRegistration) R.string.payout_save_continue else R.string.payout_save)
        findViewById<View>(R.id.payout_add).setOnClickListener { if (rows.childCount < 5) addRow() }
        cashOnly.setOnCheckedChangeListener { _, on -> rows.alpha = if (on) 0.4f else 1f; rows.isEnabled = !on }
        save.setOnClickListener { submit() }
        addRow()
        load()
    }

    private fun addRow(wallet: String = "", handle: String = ""): View {
        val v = LayoutInflater.from(this).inflate(R.layout.item_payout_row, rows, false)
        val w = v.findViewById<MaterialAutoCompleteTextView>(R.id.row_wallet)
        w.setText(wallet)
        w.setAdapter(ArrayAdapter(this, android.R.layout.simple_list_item_1, suggestions.map { it.optString("name") }))
        v.findViewById<TextInputEditText>(R.id.row_handle).setText(handle)
        v.findViewById<View>(R.id.row_remove).setOnClickListener { if (rows.childCount > 1) rows.removeView(v) }
        rows.addView(v)
        return v
    }

    private fun load() {
        ServerClient.IO_EXECUTOR.execute {
            val country = Prefs.country(this)
            val rails = ServerClient.rails(this, country)?.optJSONArray("rails")
            val current = ServerClient.payout(this)
            runOnUiThread {
                suggestions = (0 until (rails?.length() ?: 0)).mapNotNull { rails?.optJSONObject(it) }
                for (i in 0 until rows.childCount) {
                    rows.getChildAt(i).findViewById<MaterialAutoCompleteTextView>(R.id.row_wallet)
                        .setAdapter(ArrayAdapter(this, android.R.layout.simple_list_item_1, suggestions.map { it.optString("name") }))
                }
                if (current != null) {
                    holder.setText(current.optString("holder").takeIf { it != "null" } ?: "")
                    cashOnly.isChecked = current.optBoolean("cashOnly")
                    val ds = current.optJSONArray("destinations")
                    if (ds != null && ds.length() > 0) {
                        rows.removeAllViews()
                        for (i in 0 until ds.length()) {
                            val d = ds.getJSONObject(i)
                            addRow(d.optString("wallet"), d.optString("handle"))
                        }
                    }
                }
            }
        }
    }

    private fun setBusy(b: Boolean) {
        progress.visibility = if (b) View.VISIBLE else View.INVISIBLE
        save.isEnabled = !b
    }

    private fun submit() {
        error.visibility = View.GONE
        val dests = JSONArray()
        for (i in 0 until rows.childCount) {
            val v = rows.getChildAt(i)
            val w = v.findViewById<MaterialAutoCompleteTextView>(R.id.row_wallet).text.toString().trim()
            val h = v.findViewById<TextInputEditText>(R.id.row_handle).text.toString().trim()
            v.findViewById<TextInputLayout>(R.id.row_handle_til).error = null
            if (w.isEmpty() && h.isEmpty()) continue
            if (!cashOnly.isChecked && (w.isEmpty() || h.isEmpty())) {
                v.findViewById<TextInputLayout>(R.id.row_handle_til).error = getString(R.string.payout_error_row, i + 1)
                return
            }
            dests.put(JSONObject().put("wallet", w).put("handle", h))
        }
        val body = JSONObject()
            .put("holder", holder.text.toString().trim())
            .put("cashOnly", cashOnly.isChecked)
            .put("destinations", dests)
        setBusy(true)
        ServerClient.IO_EXECUTOR.execute {
            val r = ServerClient.setPayout(this, body)
            runOnUiThread {
                setBusy(false)
                if (r.code in 200..299) {
                    Toast.makeText(this, R.string.payout_saved, Toast.LENGTH_SHORT).show()
                    finishFlow()
                } else if (r.code == 422 && r.json != null) {
                    val idx = r.json.optInt("index", 0)
                    (rows.getChildAt(idx) ?: rows.getChildAt(0))?.findViewById<TextInputLayout>(R.id.row_handle_til)?.error = getString(R.string.payout_error_row, idx + 1)
                } else {
                    error.text = getString(R.string.payout_error_generic, r.json?.optString("error").orEmpty().ifEmpty { r.code.toString() })
                    error.visibility = View.VISIBLE
                }
            }
        }
    }

    private fun finishFlow() {
        if (fromRegistration) {
            startActivity(Intent(this, OnboardingActivity::class.java))
            overridePendingTransition(android.R.anim.fade_in, android.R.anim.fade_out)
        }
        finish()
    }
}
