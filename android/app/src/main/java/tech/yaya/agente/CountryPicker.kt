package tech.yaya.agente

import android.app.Activity
import android.text.Editable
import android.text.TextWatcher
import android.view.LayoutInflater
import android.view.View
import android.view.ViewGroup
import android.widget.TextView
import androidx.recyclerview.widget.LinearLayoutManager
import androidx.recyclerview.widget.RecyclerView
import com.google.android.material.bottomsheet.BottomSheetBehavior
import com.google.android.material.bottomsheet.BottomSheetDialog
import com.google.android.material.textfield.TextInputEditText

/**
 * The flag + dial-code picker (bottom sheet with search) shared by every
 * phone field: Yaya sign-in and business registration.
 */
object CountryPicker {

    fun show(activity: Activity, onPick: (Country) -> Unit) {
        val dialog = BottomSheetDialog(activity)
        val view = LayoutInflater.from(activity).inflate(R.layout.sheet_country_picker, null)
        val sheetHeight = (activity.resources.displayMetrics.heightPixels * 0.72f).toInt()
        dialog.setContentView(view, ViewGroup.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, sheetHeight))
        dialog.behavior.state = BottomSheetBehavior.STATE_EXPANDED
        dialog.behavior.skipCollapsed = true

        val list = view.findViewById<RecyclerView>(R.id.country_list)
        val empty = view.findViewById<TextView>(R.id.country_empty)
        val search = view.findViewById<TextInputEditText>(R.id.country_search_input)
        val adapter = Adapter(Countries.all(activity)) { picked -> dialog.dismiss(); onPick(picked) }
        list.layoutManager = LinearLayoutManager(activity)
        list.adapter = adapter
        search.addTextChangedListener(object : TextWatcher {
            override fun beforeTextChanged(s: CharSequence?, a: Int, b: Int, c: Int) {}
            override fun onTextChanged(s: CharSequence?, a: Int, b: Int, c: Int) {}
            override fun afterTextChanged(s: Editable?) {
                val results = Countries.search(activity, s?.toString().orEmpty())
                adapter.submit(results)
                empty.visibility = if (results.isEmpty()) View.VISIBLE else View.GONE
                list.visibility = if (results.isEmpty()) View.GONE else View.VISIBLE
            }
        })
        dialog.show()
    }

    /** "+51987654321" → "+51 987 654 321" for helper text and subtitles. */
    fun pretty(country: Country, e164: String): String {
        val digits = e164.removePrefix("+")
        // A number from another country (pasted with its own +code) is shown
        // as it is, never under the picker's code it will not be sent to.
        if (!digits.startsWith(country.dial)) return "+" + digits.chunked(3).joinToString(" ")
        val local = digits.removePrefix(country.dial)
        return "+" + country.dial + " " + local.chunked(3).joinToString(" ")
    }

    private class Adapter(initial: List<Country>, private val onPick: (Country) -> Unit) : RecyclerView.Adapter<Adapter.Holder>() {
        private var items: List<Country> = initial

        class Holder(v: View) : RecyclerView.ViewHolder(v) {
            val flag: TextView = v.findViewById(R.id.country_flag)
            val name: TextView = v.findViewById(R.id.country_name)
            val dial: TextView = v.findViewById(R.id.country_dial)
        }

        fun submit(list: List<Country>) { items = list; notifyDataSetChanged() }
        override fun onCreateViewHolder(parent: ViewGroup, viewType: Int): Holder =
            Holder(LayoutInflater.from(parent.context).inflate(R.layout.item_country, parent, false))
        override fun getItemCount() = items.size
        override fun onBindViewHolder(holder: Holder, position: Int) {
            val c = items[position]
            holder.flag.text = c.flag
            holder.name.text = c.name(holder.itemView.context)
            holder.dial.text = "+" + c.dial
            holder.itemView.setOnClickListener { onPick(c) }
        }
    }
}
