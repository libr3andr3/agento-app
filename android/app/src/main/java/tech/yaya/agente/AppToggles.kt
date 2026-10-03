package tech.yaya.agente

import android.content.Context
import android.content.pm.PackageManager
import android.graphics.drawable.Drawable
import android.view.LayoutInflater
import android.view.View
import android.widget.ImageView
import android.widget.LinearLayout
import android.widget.TextView
import androidx.core.content.ContextCompat
import com.google.android.material.materialswitch.MaterialSwitch

/**
 * One row per app — icon, name, optional sub-label, a switch
 * (`item_app_switch.xml`) — shared by the read screen ([ReadAppsActivity],
 * a RecyclerView) and the reply screen ([ReplyAppsActivity]) so the two
 * never drift apart.
 */
object AppToggles {

    fun isInstalled(ctx: Context, pkg: String): Boolean = try {
        ctx.packageManager.getPackageInfo(pkg, 0)
        true
    } catch (_: PackageManager.NameNotFoundException) {
        // Not visible to us (Android 11+ package visibility): an app this
        // phone learned proved it exists by posting notifications.
        ProfileStore.get(ctx, pkg) != null
    }

    fun appIcon(ctx: Context, pkg: String): Drawable? = try {
        ctx.packageManager.getApplicationIcon(pkg)
    } catch (_: Exception) {
        ContextCompat.getDrawable(ctx, R.drawable.ic_bell)?.apply {
            setTint(ContextCompat.getColor(ctx, R.color.agente_on_surface_muted))
        }
    }

    /**
     * Fills an inflated `item_app_switch` row. An unavailable app (not
     * installed, or not read — so nothing of it can be answered) renders
     * dimmed with the switch disabled: a toggle that cannot act is a lie.
     * Safe on a recycled row: the old listener is dropped before the state
     * is set.
     */
    fun bind(
        row: View,
        icon: Drawable?,
        name: String,
        available: Boolean,
        subLabel: String?,
        checked: Boolean,
        switchEnabled: Boolean = available,
        onToggle: (Boolean) -> Unit,
    ): MaterialSwitch {
        row.alpha = if (available) 1f else 0.45f
        row.findViewById<ImageView>(R.id.app_icon).setImageDrawable(icon)
        row.findViewById<TextView>(R.id.app_name).text = name
        row.findViewById<TextView>(R.id.app_sub).apply {
            text = subLabel
            visibility = if (subLabel == null) View.GONE else View.VISIBLE
        }
        return row.findViewById<MaterialSwitch>(R.id.app_switch).apply {
            setOnCheckedChangeListener(null)
            isEnabled = switchEnabled
            isChecked = available && checked
            contentDescription = name
            setOnCheckedChangeListener { _, on -> onToggle(on) }
        }
    }

    /** Inflates, binds and appends a row to [parent]. */
    fun addRow(
        ctx: Context,
        parent: LinearLayout,
        pkg: String,
        name: String,
        available: Boolean,
        subLabel: String?,
        checked: Boolean,
        switchEnabled: Boolean = available,
        onToggle: (Boolean) -> Unit,
    ): MaterialSwitch {
        val row = LayoutInflater.from(ctx).inflate(R.layout.item_app_switch, parent, false)
        parent.addView(row)
        return bind(row, appIcon(ctx, pkg), name, available, subLabel, checked, switchEnabled, onToggle)
    }
}
