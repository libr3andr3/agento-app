package tech.yaya.agente

import android.content.Context
import android.graphics.Canvas
import android.graphics.Paint
import android.graphics.RectF
import android.graphics.Typeface
import android.text.TextPaint
import android.util.AttributeSet
import android.view.MotionEvent
import android.view.View
import org.json.JSONObject
import java.text.SimpleDateFormat
import java.util.Calendar
import java.util.Locale
import kotlin.math.max
import kotlin.math.min

/**
 * The week on a grid (block `agenda_week`): seven day columns from today,
 * one row per hour inside the business's opening span, bookings drawn as
 * emerald blocks. Drawn by hand because it must work with the tokens and no
 * new dependency; a tap on a booking hands it back through [onTap].
 */
class WeekGridView @JvmOverloads constructor(ctx: Context, attrs: AttributeSet? = null) : View(ctx, attrs) {

    class Slot(val appt: JSONObject, val day: Int, val startMin: Int, val endMin: Int)

    var onTap: ((JSONObject) -> Unit)? = null

    private var days: List<Calendar> = emptyList()
    private var slots: List<Slot> = emptyList()
    private var openMin = 8 * 60
    private var closeMin = 20 * 60
    /** Per weekday (Calendar.DAY_OF_WEEK index 1..7) open ranges in minutes, for shading closed hours. */
    private var ranges: Map<Int, List<IntRange>> = emptyMap()
    private var slotMinutes = 30

    private val dp = resources.displayMetrics.density
    private val rowH = 40f * dp
    private val headerH = 44f * dp
    private val gutterW = 40f * dp

    private val gridPaint = Paint(Paint.ANTI_ALIAS_FLAG).apply { color = ctx.getColor(R.color.agente_outline); strokeWidth = 1f }
    private val closedPaint = Paint().apply { color = ctx.getColor(R.color.agente_surface_variant) }
    private val nowPaint = Paint(Paint.ANTI_ALIAS_FLAG).apply { color = ctx.getColor(R.color.agente_secondary); strokeWidth = 2f * dp }
    private val bookPaint = Paint(Paint.ANTI_ALIAS_FLAG).apply { color = ctx.getColor(R.color.agente_primary) }
    private val pendingPaint = Paint(Paint.ANTI_ALIAS_FLAG).apply { color = ctx.getColor(R.color.agente_secondary) }
    private val donePaint = Paint(Paint.ANTI_ALIAS_FLAG).apply { color = ctx.getColor(R.color.agente_on_surface_muted) }
    private val todayPaint = Paint().apply { color = ctx.getColor(R.color.agente_halo) }
    private val label = TextPaint(Paint.ANTI_ALIAS_FLAG).apply {
        color = ctx.getColor(R.color.agente_on_surface_muted); textSize = 11f * dp
        typeface = resources.getFont(R.font.jakarta)
    }
    private val dayLabel = TextPaint(Paint.ANTI_ALIAS_FLAG).apply {
        color = ctx.getColor(R.color.agente_on_surface); textSize = 12f * dp
        typeface = Typeface.create(resources.getFont(R.font.jakarta), Typeface.BOLD); textAlign = Paint.Align.CENTER
    }
    private val bookLabel = TextPaint(Paint.ANTI_ALIAS_FLAG).apply {
        color = ctx.getColor(R.color.agente_on_primary); textSize = 10f * dp
        typeface = Typeface.create(resources.getFont(R.font.jakarta), Typeface.BOLD)
    }

    /**
     * @param appointments dashboard rows (`date` yyyy-MM-dd, `time` HH:mm, `durationMins`, `status`, `customer`)
     * @param hours `values.businessHours` (`mon`…`sun` → "9-18", "12-15,18-22", "9:30-13:00", "cerrado")
     */
    fun bind(appointments: org.json.JSONArray?, hours: JSONObject?, slotDuration: Int) {
        slotMinutes = slotDuration.coerceIn(10, 240)
        val today = Calendar.getInstance().apply { set(Calendar.HOUR_OF_DAY, 0); set(Calendar.MINUTE, 0); set(Calendar.SECOND, 0); set(Calendar.MILLISECOND, 0) }
        days = (0 until 7).map { (today.clone() as Calendar).apply { add(Calendar.DAY_OF_YEAR, it) } }
        ranges = parseHours(hours)
        val all = ranges.values.flatten()
        openMin = (all.minOfOrNull { it.first } ?: (8 * 60)).let { it - it % 60 }
        closeMin = (all.maxOfOrNull { it.last + 1 } ?: (20 * 60)).let { if (it % 60 == 0) it else it + (60 - it % 60) }
        val fmt = SimpleDateFormat("yyyy-MM-dd", Locale.US)
        val dayKeys = days.map { fmt.format(it.time) }
        val out = ArrayList<Slot>()
        if (appointments != null) for (i in 0 until appointments.length()) {
            val a = appointments.getJSONObject(i)
            val d = dayKeys.indexOf(a.optString("date")); if (d < 0) continue
            val t = a.optString("time"); val hh = t.substringBefore(':').toIntOrNull() ?: continue
            val mm = t.substringAfter(':', "0").toIntOrNull() ?: 0
            val start = hh * 60 + mm
            val dur = a.optInt("durationMins", 0).takeIf { it > 0 } ?: slotMinutes
            out.add(Slot(a, d, start, start + dur))
            openMin = min(openMin, start - start % 60)
            closeMin = max(closeMin, ((start + dur + 59) / 60) * 60)
        }
        slots = out
        requestLayout(); invalidate()
    }

    private fun parseHours(h: JSONObject?): Map<Int, List<IntRange>> {
        val keys = listOf("sun", "mon", "tue", "wed", "thu", "fri", "sat")
        val alt = mapOf("domingo" to 0, "lunes" to 1, "martes" to 2, "miercoles" to 3, "miércoles" to 3, "jueves" to 4, "viernes" to 5, "sabado" to 6, "sábado" to 6)
        val out = HashMap<Int, List<IntRange>>()
        if (h == null) return out
        for (k in h.keys()) {
            val idx = keys.indexOf(k.lowercase().take(3)).takeIf { it >= 0 } ?: alt[k.lowercase()] ?: continue
            val v = h.opt(k)?.toString() ?: continue
            val rs = ArrayList<IntRange>()
            for (part in v.split(',', ';', '/')) {
                val m = Regex("(\\d{1,2})(?::(\\d{2}))?\\s*(?:h|hrs)?\\s*(?:-|–|a|to)\\s*(\\d{1,2})(?::(\\d{2}))?").find(part) ?: continue
                val a = m.groupValues[1].toInt() * 60 + (m.groupValues[2].toIntOrNull() ?: 0)
                var b = m.groupValues[3].toInt() * 60 + (m.groupValues[4].toIntOrNull() ?: 0)
                if (b <= a) b += 12 * 60 // "9-5"
                rs.add(a until min(b, 24 * 60))
            }
            out[idx + 1] = rs // Calendar.DAY_OF_WEEK: SUNDAY=1
        }
        return out
    }

    private fun rows(): Int = max(1, (closeMin - openMin) / 60)

    override fun onMeasure(widthMeasureSpec: Int, heightMeasureSpec: Int) {
        val w = MeasureSpec.getSize(widthMeasureSpec)
        setMeasuredDimension(w, (headerH + rows() * rowH + 8 * dp).toInt())
    }

    private fun colW(): Float = (width - gutterW) / 7f

    override fun onDraw(c: Canvas) {
        if (days.isEmpty()) return
        val cw = colW()
        val top = headerH
        val nowCal = Calendar.getInstance()
        // Day headers + closed shading + today halo.
        for (d in 0 until 7) {
            val x = gutterW + d * cw
            val cal = days[d]
            if (d == 0) c.drawRect(x, top, x + cw, top + rows() * rowH, todayPaint)
            val open = ranges[cal.get(Calendar.DAY_OF_WEEK)]
            if (open != null) {
                // Shade everything outside the open ranges.
                var cursor = openMin
                for (r in open.sortedBy { it.first }) {
                    if (r.first > cursor) c.drawRect(x, yOf(cursor, top), x + cw, yOf(r.first, top), closedPaint)
                    cursor = max(cursor, r.last + 1)
                }
                if (cursor < closeMin) c.drawRect(x, yOf(cursor, top), x + cw, yOf(closeMin, top), closedPaint)
            } else if (ranges.isNotEmpty()) {
                c.drawRect(x, top, x + cw, top + rows() * rowH, closedPaint)
            }
            val name = SimpleDateFormat("EEE", Locale.getDefault()).format(cal.time).replace(".", "").take(3)
            c.drawText(name, x + cw / 2, 16f * dp, dayLabel)
            c.drawText(cal.get(Calendar.DAY_OF_MONTH).toString(), x + cw / 2, 32f * dp, label.apply { textAlign = Paint.Align.CENTER })
        }
        label.textAlign = Paint.Align.LEFT
        // Hour lines + gutter labels.
        for (r in 0..rows()) {
            val y = top + r * rowH
            c.drawLine(gutterW, y, width.toFloat(), y, gridPaint)
            if (r < rows()) c.drawText(String.format(Locale.US, "%02d", (openMin / 60 + r) % 24), 4f * dp, y + 12f * dp, label)
        }
        for (d in 0..7) { val x = gutterW + d * cw; c.drawLine(x, top, x, top + rows() * rowH, gridPaint) }
        // Bookings.
        val pad = 2f * dp
        for (s in slots) {
            val x = gutterW + s.day * cw
            val rect = RectF(x + pad, yOf(s.startMin, top) + pad / 2, x + cw - pad, max(yOf(s.endMin, top) - pad / 2, yOf(s.startMin, top) + 14f * dp))
            val status = s.appt.optString("status")
            val paint = when {
                status == "done" || status == "no_show" -> donePaint
                status == "pending_payment" -> pendingPaint
                else -> bookPaint
            }
            c.drawRoundRect(rect, 6f * dp, 6f * dp, paint)
            val name = s.appt.optString("customer").ifBlank { "·" }
            val fit = android.text.TextUtils.ellipsize(name, bookLabel, rect.width() - 2 * pad, android.text.TextUtils.TruncateAt.END)
            c.drawText(fit.toString(), rect.left + pad, rect.top + 11f * dp, bookLabel)
        }
        // Now line on today.
        val nowMin = nowCal.get(Calendar.HOUR_OF_DAY) * 60 + nowCal.get(Calendar.MINUTE)
        if (nowMin in openMin..closeMin) {
            val y = yOf(nowMin, top)
            c.drawLine(gutterW, y, gutterW + cw, y, nowPaint)
        }
    }

    private fun yOf(minute: Int, top: Float): Float = top + (minute - openMin).coerceIn(0, closeMin - openMin) / 60f * rowH

    override fun onTouchEvent(e: MotionEvent): Boolean {
        if (e.action == MotionEvent.ACTION_UP) {
            val cw = colW(); val top = headerH
            for (s in slots) {
                val x = gutterW + s.day * cw
                val r = RectF(x, yOf(s.startMin, top), x + cw, max(yOf(s.endMin, top), yOf(s.startMin, top) + 14f * dp))
                if (r.contains(e.x, e.y)) { onTap?.invoke(s.appt); performClick(); return true }
            }
        }
        return e.action == MotionEvent.ACTION_DOWN || super.onTouchEvent(e)
    }

    override fun performClick(): Boolean { super.performClick(); return true }
}
