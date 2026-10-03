package tech.yaya.agente

import android.content.Intent
import android.graphics.Canvas
import android.graphics.Paint
import android.graphics.drawable.GradientDrawable
import android.view.View
import android.view.ViewGroup
import android.widget.ImageView
import android.widget.LinearLayout
import android.widget.TextView
import androidx.recyclerview.widget.GridLayoutManager
import androidx.recyclerview.widget.ItemTouchHelper
import androidx.recyclerview.widget.LinearLayoutManager
import androidx.recyclerview.widget.RecyclerView
import com.google.android.material.button.MaterialButton
import com.google.android.material.card.MaterialCardView
import com.google.android.material.chip.Chip
import com.google.android.material.dialog.MaterialAlertDialogBuilder
import com.google.android.material.snackbar.Snackbar
import org.json.JSONArray
import org.json.JSONObject
import java.text.SimpleDateFormat
import java.util.Calendar
import java.util.Locale

/**
 * The block catalog (DECISIONS D15). Every block the core may put in a UI
 * spec has one renderer here; the names match `server/src/ui.rs::BLOCKS`.
 * A renderer appends views to the tab's column from the dashboard payload
 * and calls back into [DashboardActivity] for anything that talks to the
 * core (status changes, photos, links).
 */
object Blocks {

    fun render(a: DashboardActivity, block: UiSpec.Block, host: LinearLayout, d: JSONObject) {
        when (block.type) {
            "earnings" -> earnings(a, host, d)
            "attention" -> attention(a, host, d)
            "orders_board" -> ordersBoard(a, host, d)
            "agenda_day" -> agendaDay(a, host, d)
            "agenda_week" -> agendaWeek(a, host, d)
            "catalog" -> catalog(a, host, d)
            "conversations" -> conversations(a, host)
            "contacts" -> contacts(a, host)
        }
    }

    // ------------------------------------------------------------ helpers

    private fun header(a: DashboardActivity, host: LinearLayout, text: String, color: Int = R.color.agente_on_surface, action: Pair<String, () -> Unit>? = null) {
        val row = LinearLayout(a).apply { orientation = LinearLayout.HORIZONTAL; gravity = android.view.Gravity.CENTER_VERTICAL }
        val m = a.resources.getDimensionPixelSize(R.dimen.space_m)
        val s = a.resources.getDimensionPixelSize(R.dimen.space_s)
        row.setPadding(s, m, 0, s)
        row.addView(TextView(a).apply {
            setTextAppearance(R.style.TextAppearance_Agente_Title)
            setTextColor(a.getColor(color)); this.text = text
            layoutParams = LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f)
        })
        if (action != null) row.addView(TextView(a).apply {
            setTextAppearance(R.style.TextAppearance_Agente_LabelLarge)
            setTextColor(a.getColor(R.color.agente_primary)); this.text = action.first
            minHeight = a.resources.getDimensionPixelSize(R.dimen.touch_min); gravity = android.view.Gravity.CENTER
            setPadding(s, 0, s, 0); setOnClickListener { action.second() }
            background = a.selectableBorderless()
        })
        host.addView(row)
    }

    private fun list(a: DashboardActivity, host: LinearLayout): RecyclerView =
        RecyclerView(a).apply {
            layoutManager = LinearLayoutManager(a); isNestedScrollingEnabled = false
            overScrollMode = View.OVER_SCROLL_NEVER
            host.addView(this, LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT))
        }

    private fun today(): String = SimpleDateFormat("yyyy-MM-dd", Locale.US).format(Calendar.getInstance().time)

    private fun rows(arr: JSONArray?): List<JSONObject> = (0 until (arr?.length() ?: 0)).map { arr!!.getJSONObject(it) }

    internal fun orderSummary(o: JSONObject): String {
        val items = o.optJSONArray("items")
        return (0 until (items?.length() ?: 0)).joinToString(", ") { j ->
            val it = items!!.getJSONObject(j); "${it.optInt("qty", 1)}× ${it.optString("product")}"
        }
    }

    // ----------------------------------------------------------- earnings

    private fun earnings(a: DashboardActivity, host: LinearLayout, d: JSONObject) {
        val v = a.inflateIn(R.layout.block_earnings, host)
        val e = d.optJSONObject("earnings") ?: JSONObject()
        v.findViewById<TextView>(R.id.earn_today).text = a.soles(e.optDouble("today", 0.0))
        v.findViewById<TextView>(R.id.earn_week).text = a.soles(e.optDouble("week", 0.0))
        v.findViewById<TextView>(R.id.earn_month).text = a.soles(e.optDouble("month", 0.0))
        host.addView(v)
    }

    // ---------------------------------------------------------- attention

    private fun attention(a: DashboardActivity, host: LinearLayout, d: JSONObject) {
        val gaps = rows(d.optJSONArray("openGaps"))
        header(a, host, a.getString(R.string.dash_attention), R.color.agente_secondary)
        if (gaps.isEmpty()) { host.addView(a.emptyState(host, a.getString(R.string.dash_gaps_empty))); return }
        for (g in gaps) {
            val card = a.inflateIn(R.layout.item_dash_gap, host)
            card.findViewById<TextView>(R.id.dash_gap_from).text = a.getString(R.string.gap_from, g.optString("customer"))
            card.findViewById<TextView>(R.id.dash_gap_question).text = g.optString("question")
            card.findViewById<MaterialButton>(R.id.dash_gap_answer).setOnClickListener { a.askGapAnswer(g.optString("id"), g.optString("question")) }
            host.addView(card)
        }
    }

    // ------------------------------------------------------- orders board

    private fun ordersBoard(a: DashboardActivity, host: LinearLayout, d: JSONObject) {
        val all = rows(d.optJSONArray("orders"))
        val toPrepare = all.filter { it.optString("status") != "done" && it.optBoolean("paid") }
        val unpaid = all.filter { it.optString("status") != "done" && !it.optBoolean("paid") }
        val doneToday = all.filter { it.optString("status") == "done" }

        header(a, host, a.getString(R.string.queue_paid_header))
        if (toPrepare.isEmpty()) host.addView(a.emptyState(host, a.getString(if (all.isEmpty()) R.string.dash_orders_empty else R.string.queue_paid_empty)))
        else {
            host.addView(hint(a, a.getString(R.string.queue_swipe_hint)))
            val rv = list(a, host)
            rv.adapter = QueueAdapter(a, toPrepare.toMutableList(), QueueAdapter.ORDER)
            attachSwipe(a, rv, QueueAdapter.ORDER)
        }
        if (unpaid.isNotEmpty()) {
            header(a, host, a.getString(R.string.queue_pending_header), R.color.agente_on_surface_muted)
            val rv = list(a, host)
            rv.adapter = QueueAdapter(a, unpaid.toMutableList(), QueueAdapter.ORDER)
            attachSwipe(a, rv, QueueAdapter.ORDER)
        }
        if (doneToday.isNotEmpty()) {
            header(a, host, a.getString(R.string.queue_done_today, doneToday.size), R.color.agente_on_surface_muted)
            val rv = list(a, host)
            rv.adapter = QueueAdapter(a, doneToday.toMutableList(), QueueAdapter.ORDER)
        }
    }

    private fun hint(a: DashboardActivity, text: String): View = TextView(a).apply {
        setTextAppearance(R.style.TextAppearance_Agente_Label); setTextColor(a.getColor(R.color.agente_on_surface_muted))
        this.text = text; val s = a.resources.getDimensionPixelSize(R.dimen.space_s); setPadding(s, 0, s, s)
    }

    // ---------------------------------------------------------- agenda day

    private fun agendaDay(a: DashboardActivity, host: LinearLayout, d: JSONObject) {
        val all = rows(d.optJSONArray("appointments"))
        val t = today()
        val todays = all.filter { it.optString("date") == t }
        val later = all.filter { it.optString("date") > t }
        header(a, host, a.getString(R.string.agenda_today_header, prettyDate(t)))
        if (todays.isEmpty()) host.addView(a.emptyState(host, a.getString(R.string.agenda_today_empty)))
        else {
            host.addView(hint(a, a.getString(R.string.agenda_swipe_hint)))
            val rv = list(a, host)
            rv.adapter = QueueAdapter(a, todays.toMutableList(), QueueAdapter.APPOINTMENT)
            attachSwipe(a, rv, QueueAdapter.APPOINTMENT)
        }
        if (later.isNotEmpty()) {
            header(a, host, a.getString(R.string.agenda_upcoming_header), R.color.agente_on_surface_muted)
            var lastDate = ""
            for (ap in later.take(30)) {
                val date = ap.optString("date")
                if (date != lastDate) { lastDate = date; host.addView(dayChip(a, host, date)) }
                host.addView(appointmentCard(a, host, ap))
            }
        }
    }

    private fun dayChip(a: DashboardActivity, host: ViewGroup, date: String): View {
        val chip = a.inflateIn(R.layout.item_dash_day, host) as Chip
        chip.text = prettyDate(date)
        a.styleChip(chip, R.color.agente_surface_variant, R.color.agente_on_surface)
        return chip
    }

    fun prettyDate(iso: String): String = try {
        SimpleDateFormat("EEE d MMM", Locale.getDefault()).format(SimpleDateFormat("yyyy-MM-dd", Locale.US).parse(iso)!!)
    } catch (_: Exception) { iso }

    fun appointmentCard(a: DashboardActivity, parent: ViewGroup, ap: JSONObject): View {
        val card = a.inflateIn(R.layout.item_dash_appointment, parent)
        bindAppointment(a, card, ap)
        card.setOnClickListener { a.appointmentActions(ap) }
        return card
    }

    fun bindAppointment(a: DashboardActivity, card: View, ap: JSONObject) {
        card.findViewById<TextView>(R.id.dash_appt_time).text = ap.optString("time")
        card.findViewById<TextView>(R.id.dash_appt_customer).text = ap.optString("customer")
        val spec = ap.optString("specialist").takeIf { it.isNotEmpty() && it != "null" }
        val service = ap.optString("service").takeIf { it.isNotEmpty() && it != "null" }
        card.findViewById<TextView>(R.id.dash_appt_sub).text = listOfNotNull(service, spec).joinToString(" · ").ifEmpty { ap.optString("phone").substringAfter(':') }
        val remEmail = ap.optString("reminderEmail").takeIf { it.isNotEmpty() && it != "null" }
        card.findViewById<TextView>(R.id.dash_appt_reminder).apply {
            if (remEmail != null) { visibility = View.VISIBLE; text = a.getString(R.string.dash_reminder_set, ap.optInt("remindMinutes", 60)) } else visibility = View.GONE
        }
        val price = ap.optDouble("price", Double.NaN)
        card.findViewById<TextView>(R.id.dash_appt_price).text = if (price.isNaN()) "—" else a.soles(price)
        val chip = card.findViewById<Chip>(R.id.dash_appt_paid)
        when {
            ap.optString("status") == "done" -> { chip.text = a.getString(R.string.queue_done); a.styleChip(chip, R.color.agente_surface_variant, R.color.agente_on_surface_muted) }
            ap.optString("status") == "no_show" -> { chip.text = a.getString(R.string.queue_no_show); a.styleChip(chip, R.color.agente_error_container, R.color.agente_error) }
            ap.optBoolean("paid") -> { chip.text = a.getString(R.string.dash_paid); a.styleChip(chip, R.color.agente_primary_container, R.color.agente_on_primary_container) }
            else -> { chip.text = a.getString(R.string.dash_pending); a.styleChip(chip, R.color.agente_secondary_container, R.color.agente_on_secondary_container) }
        }
        card.alpha = if (ap.optString("status") == "done" || ap.optString("status") == "no_show") 0.6f else 1f
    }

    // --------------------------------------------------------- agenda week

    private fun agendaWeek(a: DashboardActivity, host: LinearLayout, d: JSONObject) {
        header(a, host, a.getString(R.string.agenda_week_header), action = a.getString(R.string.agenda_export) to { a.exportIcs() })
        val card = MaterialCardView(a).apply {
            radius = a.resources.getDimension(R.dimen.corner_card); cardElevation = a.resources.getDimension(R.dimen.card_elevation)
            strokeWidth = 0; setCardBackgroundColor(a.getColor(R.color.agente_surface_card))
        }
        val grid = WeekGridView(a)
        grid.bind(d.optJSONArray("appointments"), d.optJSONObject("businessHours"), d.optInt("slotDuration", 30))
        grid.onTap = { a.appointmentActions(it) }
        card.addView(grid, ViewGroup.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT))
        host.addView(card, LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT))
        val n = rows(d.optJSONArray("appointments")).size
        host.addView(hint(a, if (n == 0) a.getString(R.string.agenda_week_empty) else a.getString(R.string.agenda_week_legend)))
    }

    // ------------------------------------------------------------ catalog

    private fun catalog(a: DashboardActivity, host: LinearLayout, d: JSONObject) {
        val v = a.inflateIn(R.layout.block_catalog, host)
        val media = rows(d.optJSONArray("media"))
        val products = d.optJSONObject("products") ?: JSONObject()
        v.findViewById<View>(R.id.catalog_add).setOnClickListener { a.pickPhotoFor(null) }
        val share = v.findViewById<View>(R.id.catalog_share)
        share.isEnabled = media.isNotEmpty()
        share.setOnClickListener { a.shareLink(emptyList(), emptyList()) }
        val grid = v.findViewById<RecyclerView>(R.id.catalog_grid)
        val empty = v.findViewById<TextView>(R.id.catalog_empty)
        if (media.isEmpty()) { grid.visibility = View.GONE; empty.visibility = View.VISIBLE } else {
            grid.visibility = View.VISIBLE; empty.visibility = View.GONE
            grid.layoutManager = GridLayoutManager(a, 3); grid.isNestedScrollingEnabled = false
            grid.adapter = PhotoAdapter(a, media)
        }
        val list = v.findViewById<LinearLayout>(R.id.catalog_products)
        val names = products.keys().asSequence().toList().sorted()
        if (names.isEmpty()) {
            list.addView(a.emptyState(list, a.getString(R.string.catalog_no_products)))
        } else {
            val withPhoto = media.mapNotNull { it.optString("product").takeIf { p -> p.isNotBlank() && p != "null" }?.lowercase() }.toSet()
            for (name in names) {
                val row = a.inflateIn(R.layout.item_catalog_product, list)
                row.findViewById<TextView>(R.id.catalog_product_name).text = name
                row.findViewById<TextView>(R.id.catalog_product_price).text = a.soles(products.optDouble(name, 0.0))
                val cam = row.findViewById<TextView>(R.id.catalog_product_photo)
                cam.alpha = if (name.lowercase() in withPhoto) 1f else 0.35f
                cam.setOnClickListener { a.pickPhotoFor(name) }
                row.setOnClickListener { if (name.lowercase() in withPhoto) a.shareLink(emptyList(), listOf(name)) else a.pickPhotoFor(name) }
                list.addView(row)
            }
        }
        host.addView(v)
    }

    private class PhotoAdapter(val a: DashboardActivity, val items: List<JSONObject>) : RecyclerView.Adapter<PhotoAdapter.VH>() {
        class VH(v: View) : RecyclerView.ViewHolder(v) {
            val img: ImageView = v.findViewById(R.id.catalog_photo_img)
            val cap: TextView = v.findViewById(R.id.catalog_photo_caption)
        }
        override fun onCreateViewHolder(parent: ViewGroup, viewType: Int) = VH(a.inflateIn(R.layout.item_catalog_photo, parent))
        override fun getItemCount() = items.size
        override fun onBindViewHolder(h: VH, i: Int) {
            val m = items[i]
            val label = m.optString("product").takeIf { it.isNotBlank() && it != "null" } ?: m.optString("caption").takeIf { it.isNotBlank() && it != "null" } ?: ""
            h.cap.text = label; h.cap.visibility = if (label.isEmpty()) View.GONE else View.VISIBLE
            a.loadPhoto(m.optString("id"), h.img)
            h.itemView.setOnClickListener { a.openPhoto(m) }
        }
    }

    // ------------------------------------------------------ conversations

    private fun conversations(a: DashboardActivity, host: LinearLayout) {
        header(a, host, a.getString(R.string.dash_convos), action = a.getString(R.string.crm_see_all) to {
            a.startActivity(Intent(a, CrmListActivity::class.java).putExtra(CrmListActivity.EXTRA_MODE, CrmListActivity.MODE_CONVERSATIONS))
        })
        val rowsArr = a.conversations
        if (rowsArr == null || rowsArr.length() == 0) { host.addView(a.emptyState(host, a.getString(R.string.dash_convos_empty))); return }
        for (i in 0 until minOf(rowsArr.length(), 6)) {
            val r = rowsArr.getJSONObject(i)
            val name = Crm.displayName(a, r.optJSONObject("contact"), r.optString("peer"))
            val card = a.inflateIn(R.layout.item_dash_convo, host) as MaterialCardView
            card.findViewById<TextView>(R.id.dash_convo_meta).text = "$name · ${Crm.shortTime(r.optString("lastAt"))}"
            card.findViewById<TextView>(R.id.dash_convo_incoming).text = r.optString("lastText")
            card.findViewById<TextView>(R.id.dash_convo_detail).apply {
                text = (if (r.optString("lastRole") == "assistant") "🤖 " else "") + a.getString(R.string.crm_messages_count, r.optInt("messages"))
                setTextColor(a.getColor(R.color.agente_on_surface_muted))
            }
            card.setOnClickListener { a.startActivity(Intent(a, ConversationActivity::class.java).putExtra(ConversationActivity.EXTRA_PEER, r.optString("peer"))) }
            host.addView(card)
        }
    }

    private fun contacts(a: DashboardActivity, host: LinearLayout) {
        val b = MaterialButton(a, null, com.google.android.material.R.attr.materialButtonOutlinedStyle).apply {
            text = a.getString(R.string.block_contacts_button)
            setOnClickListener { a.startActivity(Intent(a, CrmListActivity::class.java).putExtra(CrmListActivity.EXTRA_MODE, CrmListActivity.MODE_CONTACTS)) }
        }
        val lp = LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, a.resources.getDimensionPixelSize(R.dimen.cta_height))
        lp.topMargin = a.resources.getDimensionPixelSize(R.dimen.space_m)
        host.addView(b, lp)
    }

    // --------------------------------------------------------- the queue

    /** Orders and today's appointments share one adapter: a card, a tap for actions, a swipe for done. */
    class QueueAdapter(val a: DashboardActivity, val items: MutableList<JSONObject>, val kind: Int) : RecyclerView.Adapter<QueueAdapter.VH>() {
        companion object { const val ORDER = 1; const val APPOINTMENT = 2 }
        class VH(v: View) : RecyclerView.ViewHolder(v)
        override fun onCreateViewHolder(parent: ViewGroup, viewType: Int) =
            VH(a.inflateIn(if (kind == ORDER) R.layout.item_dash_order else R.layout.item_dash_appointment, parent))
        override fun getItemCount() = items.size
        override fun onBindViewHolder(h: VH, i: Int) {
            val o = items[i]
            if (kind == ORDER) {
                h.itemView.findViewById<TextView>(R.id.dash_order_customer).text = o.optString("customer")
                val time = o.optString("time").takeIf { it.isNotBlank() }?.let { "$it · " } ?: ""
                h.itemView.findViewById<TextView>(R.id.dash_order_items).text = time + orderSummary(o)
                h.itemView.findViewById<TextView>(R.id.dash_order_total).text = a.soles(o.optDouble("total", 0.0))
                val state = h.itemView.findViewById<Chip>(R.id.dash_order_state)
                when {
                    o.optString("status") == "done" -> { state.text = a.getString(R.string.queue_done); a.styleChip(state, R.color.agente_surface_variant, R.color.agente_on_surface_muted) }
                    o.optBoolean("paid") -> { state.text = a.getString(R.string.order_paid); a.styleChip(state, R.color.agente_primary_container, R.color.agente_on_primary_container) }
                    else -> { state.text = a.getString(R.string.order_pending); a.styleChip(state, R.color.agente_secondary_container, R.color.agente_on_secondary_container) }
                }
                h.itemView.alpha = if (o.optString("status") == "done") 0.6f else 1f
                h.itemView.setOnClickListener { a.orderActions(o) }
            } else {
                bindAppointment(a, h.itemView, o)
                h.itemView.setOnClickListener { a.appointmentActions(o) }
            }
        }
        fun remove(pos: Int): JSONObject { val o = items.removeAt(pos); notifyItemRemoved(pos); return o }
        fun restore(pos: Int, o: JSONObject) { items.add(pos.coerceAtMost(items.size), o); notifyItemInserted(pos.coerceAtMost(items.size - 1)) }
    }

    /** Swipe right = listo. Green field with a check drawn under the card while it moves. */
    private fun attachSwipe(a: DashboardActivity, rv: RecyclerView, kind: Int) {
        val bg = GradientDrawable().apply { setColor(a.getColor(R.color.agente_primary)); cornerRadius = a.resources.getDimension(R.dimen.corner_card) }
        val paint = Paint(Paint.ANTI_ALIAS_FLAG).apply {
            color = a.getColor(R.color.agente_on_primary); textSize = 16 * a.resources.displayMetrics.density
            typeface = android.graphics.Typeface.create(a.resources.getFont(R.font.jakarta), android.graphics.Typeface.BOLD)
        }
        val label = a.getString(R.string.queue_swipe_label)
        ItemTouchHelper(object : ItemTouchHelper.SimpleCallback(0, ItemTouchHelper.RIGHT) {
            override fun onMove(r: RecyclerView, v: RecyclerView.ViewHolder, t: RecyclerView.ViewHolder) = false
            override fun getSwipeThreshold(viewHolder: RecyclerView.ViewHolder) = 0.4f
            override fun onSwiped(vh: RecyclerView.ViewHolder, dir: Int) {
                val adapter = rv.adapter as QueueAdapter
                val pos = vh.bindingAdapterPosition; if (pos < 0) return
                val item = adapter.remove(pos)
                if (item.optString("status") == "done") { adapter.restore(pos, item); return }
                a.markDone(kind == QueueAdapter.ORDER, item, undo = { adapter.restore(pos, item) })
            }
            override fun onChildDraw(c: Canvas, r: RecyclerView, vh: RecyclerView.ViewHolder, dX: Float, dY: Float, state: Int, active: Boolean) {
                val v = vh.itemView
                if (dX > 0) {
                    val m = a.resources.getDimensionPixelSize(R.dimen.space_s)
                    bg.setBounds(v.left, v.top, v.left + dX.toInt(), v.bottom - m)
                    bg.draw(c)
                    if (dX > paint.measureText(label) + 3 * m) c.drawText(label, v.left + 2f * m, (v.top + v.bottom - m) / 2f + paint.textSize / 3, paint)
                }
                super.onChildDraw(c, r, vh, dX, dY, state, active)
            }
        }).attachToRecyclerView(rv)
    }
}
