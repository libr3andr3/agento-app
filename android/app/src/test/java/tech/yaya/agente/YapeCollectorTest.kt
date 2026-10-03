package tech.yaya.agente

import android.app.Notification
import android.content.Context
import android.os.Process
import android.service.notification.StatusBarNotification
import androidx.test.core.app.ApplicationProvider
import org.json.JSONObject
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import java.net.ServerSocket
import kotlin.concurrent.thread
import java.util.concurrent.CopyOnWriteArrayList

@RunWith(RobolectricTestRunner::class)
class YapeCollectorTest {
    private lateinit var ctx: Context
    private val key = "k".repeat(40)
    private var server: ServerSocket? = null
    private val got = CopyOnWriteArrayList<Pair<String, JSONObject>>()

    @Before fun setUp() {
        ctx = ApplicationProvider.getApplicationContext()
        ctx.getSharedPreferences("yape_collector", Context.MODE_PRIVATE).edit().clear().commit()
        YapeCollector.baseUrl = "http://127.0.0.1:9" // nothing listens: offline
    }

    @After fun tearDown() { server?.close() }

    /** A gateway on localhost answering [code] (one request per connection). */
    private fun gateway(code: Int) {
        server?.close()
        val ss = ServerSocket(0)
        server = ss
        thread(isDaemon = true) {
            while (!ss.isClosed) {
                val sock = runCatching { ss.accept() }.getOrNull() ?: break
                sock.use {
                    val input = it.getInputStream()
                    val head = StringBuilder()
                    while (!head.endsWith("\r\n\r\n")) { val c = input.read(); if (c < 0) break; head.append(c.toChar()) }
                    val headers = head.lines()
                    val len = headers.firstOrNull { h -> h.lowercase().startsWith("content-length:") }?.substringAfter(':')?.trim()?.toInt() ?: 0
                    val body = ByteArray(len).also { b -> var off = 0; while (off < len) { val r = input.read(b, off, len - off); if (r < 0) break; off += r } }
                    val key = headers.firstOrNull { h -> h.lowercase().startsWith("x-collector-key:") }?.substringAfter(':')?.trim().orEmpty()
                    got += key to JSONObject(body.decodeToString())
                    it.getOutputStream().write("HTTP/1.1 $code X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".toByteArray())
                }
            }
        }
        YapeCollector.baseUrl = "http://127.0.0.1:${ss.localPort}"
    }

    @Suppress("DEPRECATION")
    private fun sbn(pkg: String, text: String = "Yape! Ana te envió un pago por S/ 20.37", whenMs: Long = 1_790_000_000_000L): StatusBarNotification {
        val n = Notification.Builder(ctx, "c").setContentTitle("Confirmación de Pago").setContentText(text)
            .setSmallIcon(android.R.drawable.ic_dialog_info).setWhen(whenMs).build()
        return StatusBarNotification(pkg, pkg, 1, null, 0, 0, 0, n, Process.myUserHandle(), whenMs + 500)
    }

    @Test fun onlyYapeAndNeverSummariesMakeAPayload() {
        assertNull(YapeCollector.payload("com.whatsapp", 1, 2, false, "Ana", "te envió S/ 20", null))
        assertNull(YapeCollector.payload(YapeCollector.YAPE, 1, 2, true, "Yape", "2 pagos", null))
        assertNull(YapeCollector.payload(YapeCollector.YAPE, 1, 2, false, "", " ", null))
        val p = YapeCollector.payload(YapeCollector.YAPE, 0, 2, false, "t", "x", "big")!!
        assertEquals("postTime when the notification has no time of its own", 2L, p.getLong("postedAt"))
        assertEquals("big", p.getString("bigText"))
    }

    @Test fun offUntilTheHousePhoneIsGivenAKey() {
        YapeCollector.onPosted(ctx, sbn(YapeCollector.YAPE))
        assertEquals(0, YapeCollector.queued(ctx))
        YapeCollector.setKey(ctx, key)
        YapeCollector.onPosted(ctx, sbn("com.whatsapp"))
        assertEquals("other apps are never collected", 0, YapeCollector.queued(ctx))
        YapeCollector.onPosted(ctx, sbn(YapeCollector.YAPE))
        YapeCollector.flush(ctx)
        assertEquals("kept while the gateway is unreachable", 1, YapeCollector.queued(ctx))
    }

    @Test fun theQueueDrainsInOrderAndSurvivesServerErrors() {
        YapeCollector.setKey(ctx, key)
        YapeCollector.onPosted(ctx, sbn(YapeCollector.YAPE, "A te envió S/ 1.01", 1L))
        YapeCollector.onPosted(ctx, sbn(YapeCollector.YAPE, "B te envió S/ 2.02", 2L))
        YapeCollector.flush(ctx)
        assertEquals(2, YapeCollector.queued(ctx))
        gateway(503)
        YapeCollector.flush(ctx)
        assertEquals("a 5xx keeps everything", 2, YapeCollector.queued(ctx))
        gateway(200)
        YapeCollector.flush(ctx)
        assertEquals(0, YapeCollector.queued(ctx))
        val sent = got.filter { it.second.getString("text").contains("S/") }.takeLast(2)
        assertEquals(listOf(1L, 2L), sent.map { it.second.getLong("postedAt") })
        assertEquals(key, sent[0].first)
        assertEquals(YapeCollector.YAPE, sent[0].second.getString("package"))
    }

    @Test fun aWrongKeyKeepsThePaymentsARefusalDropsOne() {
        YapeCollector.setKey(ctx, key)
        gateway(401)
        YapeCollector.onPosted(ctx, sbn(YapeCollector.YAPE))
        YapeCollector.flush(ctx)
        assertEquals("a wrong key is fixable: keep", 1, YapeCollector.queued(ctx))
        gateway(422)
        YapeCollector.flush(ctx)
        assertEquals("a refusal would block the queue forever: drop", 0, YapeCollector.queued(ctx))
        assertNotNull(got.firstOrNull())
    }
}
