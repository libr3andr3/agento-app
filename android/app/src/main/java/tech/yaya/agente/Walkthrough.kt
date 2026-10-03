package tech.yaya.agente

import android.view.LayoutInflater
import android.view.View
import android.view.ViewGroup
import android.widget.FrameLayout
import android.widget.LinearLayout
import android.widget.TextView
import androidx.appcompat.app.AppCompatActivity
import com.google.android.material.button.MaterialButton

/**
 * The first-open guide (D15): one step per tab, in the order the agent laid
 * them out, using the agent's own `intro` line for that tab (or the
 * block-catalog default). Each step switches the bottom bar to its tab so
 * the owner sees the real screen behind the card — never a mockup.
 */
class Walkthrough(
    private val activity: AppCompatActivity,
    private val spec: UiSpec,
    private val selectTab: (Int) -> Unit,
    private val onFinished: () -> Unit,
) {
    private var step = 0
    private var overlay: View? = null

    fun show() {
        if (spec.tabs.isEmpty() || overlay != null) return
        val root = activity.findViewById<ViewGroup>(android.R.id.content)
        val v = LayoutInflater.from(activity).inflate(R.layout.overlay_walkthrough, root, false)
        overlay = v
        root.addView(v, FrameLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT))
        v.findViewById<View>(R.id.walk_skip).setOnClickListener { finish() }
        v.findViewById<View>(R.id.walk_next).setOnClickListener {
            if (step + 1 >= spec.tabs.size) finish() else { step++; render() }
        }
        v.findViewById<View>(R.id.walk_card).alpha = 0f
        v.findViewById<View>(R.id.walk_card).animate().alpha(1f).setDuration(240).start()
        render()
    }

    private fun render() {
        val v = overlay ?: return
        val tab = spec.tabs[step]
        selectTab(step)
        v.findViewById<TextView>(R.id.walk_step).text = activity.getString(R.string.walk_step, step + 1, spec.tabs.size)
        v.findViewById<TextView>(R.id.walk_title).text = tab.label
        v.findViewById<TextView>(R.id.walk_body).text = tab.intro ?: defaultIntro(tab)
        v.findViewById<MaterialButton>(R.id.walk_next).text =
            activity.getString(if (step + 1 >= spec.tabs.size) R.string.walk_done else R.string.walk_next)
        val dots = v.findViewById<LinearLayout>(R.id.walk_dots)
        dots.removeAllViews()
        val d = activity.resources.displayMetrics.density
        for (i in spec.tabs.indices) {
            dots.addView(View(activity).apply {
                layoutParams = LinearLayout.LayoutParams((8 * d).toInt(), (8 * d).toInt()).apply { marginEnd = (6 * d).toInt() }
                background = android.graphics.drawable.GradientDrawable().apply {
                    shape = android.graphics.drawable.GradientDrawable.OVAL
                    setColor(activity.getColor(if (i == step) R.color.agente_primary else R.color.agente_outline))
                }
            })
        }
    }

    /** What a tab does, from its first block, when the agent wrote no intro. */
    private fun defaultIntro(tab: UiSpec.Tab): String {
        val first = tab.blocks.firstOrNull { it.type != "earnings" && it.type != "attention" }?.type ?: tab.blocks.first().type
        return activity.getString(when (first) {
            "orders_board" -> R.string.walk_orders
            "agenda_day" -> R.string.walk_agenda_day
            "agenda_week" -> R.string.walk_agenda_week
            "catalog" -> R.string.walk_catalog
            "conversations", "contacts" -> R.string.walk_customers
            else -> R.string.walk_home
        })
    }

    private fun finish() {
        val v = overlay ?: return
        overlay = null
        (v.parent as? ViewGroup)?.removeView(v)
        selectTab(spec.homeIndex())
        onFinished()
    }

    fun isShowing() = overlay != null
}
