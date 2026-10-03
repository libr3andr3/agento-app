package tech.yaya.agente

import android.app.Activity
import android.app.AlertDialog
import android.os.Bundle

/**
 * Turns this phone into agento's till ([YapeCollector]) — or off again.
 *
 * Opened by the link `agente://collector?key=<YAPE_COLLECTOR_KEY>` (or
 * `?off=1`), sent to the house phone by the operator. Exported so the link
 * works, which means any app could open it: nothing changes without the
 * person holding the phone tapping "Activar", and the notifications can
 * only ever go to our gateway (compiled in), never to the link's author.
 */
class CollectorActivity : Activity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val uri = intent?.data
        val key = uri?.getQueryParameter("key")?.trim().orEmpty()
        val off = uri?.getQueryParameter("off") == "1"
        val b = AlertDialog.Builder(this).setOnDismissListener { finish() }
        when {
            off -> b.setTitle(R.string.collector_off_title)
                .setMessage(R.string.collector_off_body)
                .setPositiveButton(R.string.collector_off_yes) { _, _ -> YapeCollector.setKey(this, null) }
                .setNegativeButton(android.R.string.cancel, null)
            key.length >= 32 -> b.setTitle(R.string.collector_on_title)
                .setMessage(getString(R.string.collector_on_body, BuildConfig.GATEWAY_URL))
                .setPositiveButton(R.string.collector_on_yes) { _, _ ->
                    YapeCollector.setKey(this, key)
                    ServerClient.IO_EXECUTOR.execute { YapeCollector.flush(applicationContext) }
                }
                .setNegativeButton(android.R.string.cancel, null)
            else -> b.setTitle(R.string.collector_on_title)
                .setMessage(if (YapeCollector.isOn(this)) R.string.collector_status_on else R.string.collector_bad_link)
                .setPositiveButton(android.R.string.ok, null)
        }
        b.show()
    }
}
