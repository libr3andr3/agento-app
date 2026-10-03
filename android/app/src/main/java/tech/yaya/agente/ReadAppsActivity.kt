package tech.yaya.agente

import android.content.Intent
import android.graphics.drawable.Drawable
import android.os.Bundle
import android.view.LayoutInflater
import android.view.View
import android.view.ViewGroup
import android.widget.TextView
import androidx.appcompat.app.AppCompatActivity
import androidx.core.widget.doAfterTextChanged
import androidx.recyclerview.widget.ConcatAdapter
import androidx.recyclerview.widget.LinearLayoutManager
import androidx.recyclerview.widget.RecyclerView
import com.google.android.material.button.MaterialButton
import com.google.android.material.materialswitch.MaterialSwitch
import com.google.android.material.textfield.TextInputEditText

/**
 * Step 1 of 2 after the interview: which apps the agent may read
 * notifications from, one switch per app on this phone ([PhoneApps]). An
 * app switched off is dropped by the listener before anything looks at it
 * ([Prefs.canRead]). Step 2, [ReplyAppsActivity], picks where the agent
 * answers among the chat apps read here.
 *
 * First run suggests the business's chat app, every wallet and bank and
 * every app this phone learned; everything else is the owner's to switch
 * on. From Settings ([EXTRA_EDIT]) the switches show what the agent reads
 * today. Each flip is saved at once; leaving with the button also records
 * the apps the owner left untouched, so what they saw is what holds.
 */
class ReadAppsActivity : AppCompatActivity() {

    companion object {
        const val EXTRA_EDIT = "edit"
    }

    private var edit = false
    private var apps: List<PhoneApps.App> = emptyList()
    private var shown: List<PhoneApps.App> = emptyList()
    private val reads = HashMap<String, Boolean>()
    private var readLater = false
    private val icons = HashMap<String, Drawable?>()

    private lateinit var rows: RowAdapter
    private lateinit var footer: FooterAdapter
    private lateinit var search: TextInputEditText
    private lateinit var count: TextView
    private lateinit var all: MaterialButton
    private lateinit var cta: MaterialButton

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(R.layout.activity_read_apps)
        edit = intent.getBooleanExtra(EXTRA_EDIT, false)
        findViewById<TextView>(R.id.read_step).visibility = if (edit) View.GONE else View.VISIBLE
        search = findViewById(R.id.read_search)
        count = findViewById(R.id.read_count)
        all = findViewById(R.id.read_all)
        cta = findViewById(R.id.read_cta)
        cta.setText(if (edit) R.string.apps_save else R.string.read_next)
        cta.setOnClickListener { finishStep() }
        all.setOnClickListener { setAllShown(!allShownOn()) }
        search.doAfterTextChanged { filter() }
        readLater = if (edit || Prefs.readOtherSourcesChosen(this)) Prefs.readOtherSources(this) else false

        rows = RowAdapter()
        footer = FooterAdapter()
        findViewById<RecyclerView>(R.id.read_list).apply {
            layoutManager = LinearLayoutManager(this@ReadAppsActivity)
            adapter = ConcatAdapter(rows, footer)
        }

        // Every app on a full phone, with its label, takes a moment to list.
        val ctx = applicationContext
        ServerClient.IO_EXECUTOR.execute {
            val list = PhoneApps.list(ctx)
            val installed = list.map { it.packageName }
            val initial = list.associate { a ->
                a.packageName to (Prefs.readChoice(ctx, a.packageName)
                    ?: if (edit) Prefs.canRead(ctx, a.packageName) else PhoneApps.suggested(a, installed))
            }
            runOnUiThread {
                if (isFinishing || isDestroyed) return@runOnUiThread
                apps = list
                reads.putAll(initial)
                findViewById<View>(R.id.read_loading).visibility = View.GONE
                cta.isEnabled = true
                footer.notifyDataSetChanged()
                filter()
            }
        }
    }

    private fun filter() {
        val needle = search.text?.toString()?.trim().orEmpty()
        shown = if (needle.isEmpty()) apps
                else apps.filter { it.label.contains(needle, ignoreCase = true) || it.packageName.contains(needle, ignoreCase = true) }
        rows.notifyDataSetChanged()
        refreshCounts()
    }

    private fun allShownOn() = shown.isNotEmpty() && shown.all { reads[it.packageName] == true }

    private fun refreshCounts() {
        count.text = getString(R.string.read_count, apps.count { reads[it.packageName] == true }, apps.size)
        all.setText(if (allShownOn()) R.string.read_all_off else R.string.read_all_on)
        all.isEnabled = shown.isNotEmpty()
    }

    private fun set(pkg: String, on: Boolean) {
        reads[pkg] = on
        Prefs.setCanRead(this, pkg, on)
        refreshCounts()
    }

    /** "Todas" / "Ninguna" acts on what the search shows, not on hidden apps. */
    private fun setAllShown(on: Boolean) {
        shown.forEach { reads[it.packageName] = on }
        Prefs.setCanRead(this, shown.associate { it.packageName to on })
        rows.notifyDataSetChanged()
        refreshCounts()
    }

    private fun finishStep() {
        Prefs.setCanRead(this, apps.associate { it.packageName to (reads[it.packageName] == true) })
        Prefs.setReadOtherSources(this, readLater)
        if (edit) { finish(); return }
        // Kept on the back stack: back from step 2 returns here.
        startActivity(Intent(this, ReplyAppsActivity::class.java))
        overridePendingTransition(android.R.anim.fade_in, android.R.anim.fade_out)
    }

    private fun subLabel(kind: PhoneApps.Kind): String? = when (kind) {
        PhoneApps.Kind.CHAT -> getString(R.string.read_kind_chat)
        PhoneApps.Kind.LEARNED -> getString(R.string.read_kind_learned)
        PhoneApps.Kind.MONEY -> getString(R.string.read_kind_money)
        PhoneApps.Kind.OTHER -> null
    }

    private class Holder(v: View) : RecyclerView.ViewHolder(v)

    private inner class RowAdapter : RecyclerView.Adapter<Holder>() {
        override fun getItemCount() = shown.size

        override fun onCreateViewHolder(parent: ViewGroup, viewType: Int) =
            Holder(LayoutInflater.from(parent.context).inflate(R.layout.item_app_switch, parent, false))

        override fun onBindViewHolder(holder: Holder, position: Int) {
            val app = shown[position]
            val icon = icons.getOrPut(app.packageName) { AppToggles.appIcon(this@ReadAppsActivity, app.packageName) }
            AppToggles.bind(
                holder.itemView, icon, app.label, available = true, subLabel = subLabel(app.kind),
                checked = reads[app.packageName] == true,
            ) { on -> set(app.packageName, on) }
        }
    }

    /** "Apps I install later" and the privacy line, once the list is in. */
    private inner class FooterAdapter : RecyclerView.Adapter<Holder>() {
        override fun getItemCount() = if (apps.isEmpty()) 0 else 1

        override fun onCreateViewHolder(parent: ViewGroup, viewType: Int) =
            Holder(LayoutInflater.from(parent.context).inflate(R.layout.item_read_footer, parent, false))

        override fun onBindViewHolder(holder: Holder, position: Int) {
            holder.itemView.findViewById<MaterialSwitch>(R.id.read_later_switch).apply {
                setOnCheckedChangeListener(null)
                isChecked = readLater
                setOnCheckedChangeListener { _, on ->
                    readLater = on
                    Prefs.setReadOtherSources(this@ReadAppsActivity, on)
                }
            }
        }
    }
}
