package tech.yaya.agente

import android.Manifest
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Build
import androidx.core.app.NotificationCompat
import androidx.core.app.NotificationManagerCompat

/**
 * Local notifications to the business owner when the agent needs them:
 * a question it couldn't answer, or a customer asking for a human.
 * Tapping opens the dashboard, where the question sits with an answer box.
 */
object OwnerAlerts {
    private const val CHANNEL = "agente_attention"
    private const val GROUP = "tech.yaya.agente.ATTENTION"
    private const val SUMMARY_ID = -1962

    /**
     * Safe to call any number of times: createNotificationChannel is a no-op
     * when the channel already exists (user tweaks to importance are kept).
     */
    fun ensureChannel(ctx: Context) {
        if (Build.VERSION.SDK_INT < 26) return
        val mgr = ctx.getSystemService(NotificationManager::class.java) ?: return
        mgr.createNotificationChannel(
            NotificationChannel(
                CHANNEL,
                ctx.getString(R.string.notif_channel_name),
                NotificationManager.IMPORTANCE_HIGH
            )
        )
    }

    fun canPost(ctx: Context): Boolean =
        Build.VERSION.SDK_INT < 33 ||
            ctx.checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) ==
            PackageManager.PERMISSION_GRANTED

    /** `url`: when set (a top-up link), tapping opens it instead of the dashboard. */
    fun notify(ctx: Context, urgent: Boolean, sender: String, question: String, gapId: String, url: String? = null) {
        if (!canPost(ctx)) return
        // Defensive: the channel is created at app start, but the listener
        // process can outlive data-clears and OEM restarts — recreate if gone.
        ensureChannel(ctx)
        val intent = (if (url != null) Intent(Intent.ACTION_VIEW, android.net.Uri.parse(url)) else Intent(ctx, Screens.HOME))
            .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TOP)
        val pi = PendingIntent.getActivity(
            ctx, gapId.hashCode(), intent,
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT
        )
        val title = ctx.getString(
            if (urgent) R.string.notif_escalation_title else R.string.notif_attention_title
        )
        val body = "$sender: $question"
        val n = NotificationCompat.Builder(ctx, CHANNEL)
            .setSmallIcon(R.drawable.ic_bell)
            .setContentTitle(title)
            .setContentText(body)
            .setStyle(NotificationCompat.BigTextStyle().bigText(body))
            .setPriority(NotificationCompat.PRIORITY_HIGH)
            .setAutoCancel(true)
            .setContentIntent(pi)
            .setGroup(GROUP)
            .build()
        // A busy inbox raises several alerts; the summary makes them one
        // expandable stack in the shade instead of a pile.
        val summary = NotificationCompat.Builder(ctx, CHANNEL)
            .setSmallIcon(R.drawable.ic_bell)
            .setContentTitle(ctx.getString(R.string.notif_attention_title))
            .setGroup(GROUP)
            .setGroupSummary(true)
            .setAutoCancel(true)
            .setContentIntent(pi)
            .build()
        try {
            val mgr = NotificationManagerCompat.from(ctx)
            mgr.notify(gapId.hashCode(), n)
            mgr.notify(SUMMARY_ID, summary)
        } catch (_: SecurityException) {
            // Permission revoked between the check and the post — drop quietly.
        }
    }
}
