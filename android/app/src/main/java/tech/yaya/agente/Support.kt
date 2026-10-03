package tech.yaya.agente

import android.content.Context
import android.content.Intent
import android.net.Uri
import android.widget.Toast

/** "Hablar con soporte": a WhatsApp to the line the server currently names. */
object Support {
    fun open(ctx: Context) {
        val text = Uri.encode(ctx.getString(R.string.support_text))
        try {
            ctx.startActivity(Intent(Intent.ACTION_VIEW, Uri.parse("https://wa.me/${Prefs.supportPhone(ctx)}?text=$text")).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK))
        } catch (e: Exception) {
            Toast.makeText(ctx, R.string.plan_error, Toast.LENGTH_SHORT).show()
        }
    }
}
