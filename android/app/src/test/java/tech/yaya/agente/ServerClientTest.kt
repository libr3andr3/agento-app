package tech.yaya.agente

import android.content.Context
import androidx.test.core.app.ApplicationProvider
import org.json.JSONObject
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.shadows.ShadowLog
import java.net.InetAddress
import java.net.ServerSocket
import kotlin.concurrent.thread
import java.util.concurrent.CopyOnWriteArrayList

@RunWith(RobolectricTestRunner::class)
class ServerClientTest {
    private lateinit var ctx: Context
    private lateinit var server: ServerSocket
    private val seen = CopyOnWriteArrayList<Triple<String, Map<String, String>, String>>()
    /** path -> list of (status, body) replies; the last one repeats. */
    private val script = HashMap<String, MutableList<Pair<Int, String>>>()

    /** A one-request-per-connection HTTP/1.1 fake of the core. */
    private fun serve() = thread(isDaemon = true) {
        while (!server.isClosed) {
            val sock = runCatching { server.accept() }.getOrNull() ?: break
            sock.use { s ->
                val input = s.getInputStream().buffered()
                fun line(): String { val b = StringBuilder(); while (true) { val c = input.read(); if (c < 0 || c == '\n'.code) break; if (c != '\r'.code) b.append(c.toChar()) }; return b.toString() }
                val request = line()
                val headers = HashMap<String, String>()
                while (true) { val h = line(); if (h.isEmpty()) break; val i = h.indexOf(':'); if (i > 0) headers[h.substring(0, i).trim().lowercase()] = h.substring(i + 1).trim() }
                val len = headers["content-length"]?.toIntOrNull() ?: 0
                val body = ByteArray(len).also { var o = 0; while (o < len) { val n = input.read(it, o, len - o); if (n < 0) break; o += n } }.toString(Charsets.UTF_8)
                val (method, path) = request.split(" ").let { it[0] to it[1].substringBefore('?') }
                seen.add(Triple("$method $path", headers, body))
                val q = synchronized(script) { script[path] }
                val (st, out) = if (q == null) 404 to "{}" else synchronized(script) { if (q.size > 1) q.removeAt(0) else q[0] }
                val bytes = out.toByteArray()
                s.getOutputStream().apply {
                    write("HTTP/1.1 $st X\r\nContent-Type: application/json\r\nContent-Length: ${bytes.size}\r\nConnection: close\r\n\r\n".toByteArray())
                    write(bytes); flush()
                }
            }
        }
    }

    @Before fun setUp() {
        ctx = ApplicationProvider.getApplicationContext()
        server = ServerSocket(0, 50, InetAddress.getByName("127.0.0.1"))
        serve()
        ServerClient.testBaseUrl = "http://127.0.0.1:${server.localPort}"
        ServerClient.testAppKey = "app-key-for-tests"
        ShadowLog.clear()
    }

    @After fun tearDown() { server.close(); ServerClient.testBaseUrl = null; ServerClient.testAppKey = null }

    private fun hits(path: String) = seen.count { it.first.endsWith(path) }

    @Test fun anErrorBodyNeverReachesLogcat() {
        script["/api/mesh/link"] = mutableListOf(400 to """{"error":"Juan Pérez dijo: quiero 2 pollos a Av. Lima 123"}""")
        assertNull(ServerClient.meshLink(ctx, "agent:x"))
        val logged = ShadowLog.getLogs().joinToString("\n") { "${it.tag}: ${it.msg}" }
        assertFalse(logged, logged.contains("pollos") || logged.contains("Juan"))
        assertTrue("the status is still logged", logged.contains("400"))
    }

    @Test fun onlyIdempotentReadsAreRetried() {
        script["/api/wallets"] = mutableListOf(500 to "{}", 200 to """{"wallets":[]}""")
        assertTrue(ServerClient.wallets(ctx)!!.has("wallets"))
        assertEquals("an idempotent GET gets one more try", 2, hits("/api/wallets"))
        script["/api/topup/session"] = mutableListOf(500 to "{}")
        assertEquals(500, ServerClient.topupSession(ctx, 2000, "yape").code)
        assertEquals("a side-effectful POST is sent once", 1, hits("/api/topup/session"))
        script["/api/account/otp/start"] = mutableListOf(429 to """{"error":"slow down"}""")
        assertEquals(429, ServerClient.accountOtpStart(ctx, "a@b.pe", "51999", null).code)
        assertEquals("a real status is an answer, not a network failure", 1, hits("/api/account/otp/start"))
    }

    @Test fun everyRequestCarriesTheAppKeyAndACustomerTurnCarriesItsIdentity() {
        script["/api/execute_action"] = mutableListOf(200 to """{"agentResponse":"¡Hola!"}""")
        val ref = SenderRef.from(Channel.of("com.whatsapp"), "+51 999 888 777")
        assertEquals("¡Hola!", ServerClient.executeAction(ctx, "com.whatsapp:+51 999 888 777", "hola", ref)!!.getString("agentResponse"))
        val (line, headers, body) = seen.last()
        assertEquals("POST /api/execute_action", line)
        assertEquals("app-key-for-tests", headers["x-app-key"])
        assertEquals("application/json", headers["content-type"])
        val j = JSONObject(body)
        assertEquals(listOf("com.whatsapp:+51 999 888 777", "hola", "whatsapp", "51999888777", true),
            listOf(j.getString("phoneNumber"), j.getString("message"), j.getString("channel"), j.getString("handle"), j.getBoolean("handleIsPhone")))
        assertFalse("no client-supplied history", j.has("history"))
    }

    @Test fun anUnreachableCoreIsCodeZero() {
        server.close()
        assertEquals(0, ServerClient.topupSession(ctx, 2000, "yape").code)
        assertEquals(ServerClient.Kind.OFFLINE, ServerClient.classify(0))
        assertEquals(ServerClient.Kind.AUTH, ServerClient.classify(403))
        assertEquals(ServerClient.Kind.RATE_LIMITED, ServerClient.classify(429))
        assertEquals(ServerClient.Kind.UNAVAILABLE, ServerClient.classify(503))
        assertEquals(ServerClient.Kind.SERVER_DOWN, ServerClient.classify(502))
        assertEquals(ServerClient.Kind.BAD, ServerClient.classify(422))
        assertEquals(ServerClient.Kind.OK, ServerClient.classify(ServerClient.Response(204, null)))
    }

    @Test fun theWebsiteScreenTalksToTheOwnersCore() {
        script["/api/ops"] = mutableListOf(200 to """{"domain":"example.com","secret":"whsec_x","example":"https://example.com/perfil/p7k2m9x4qa"}""")
        assertEquals("example.com", ServerClient.ops(ctx)!!.getString("domain"))
        val r = ServerClient.setOps(ctx, WebsiteForm.body("example.com", "", "", true))
        assertEquals(200, r.code)
        val post = seen.last { it.first == "POST /api/ops" }
        assertTrue("owner only: the device token rides along", post.second["authorization"]!!.startsWith("Bearer"))
        assertEquals("example.com", JSONObject(post.third).getString("domain"))

        script["/api/ops/secret"] = mutableListOf(200 to """{"secret":"whsec_new"}""")
        assertEquals("whsec_new", ServerClient.opsRotateSecret(ctx).json!!.getString("secret"))
        script["/api/ops/test"] = mutableListOf(500 to "{}")
        assertEquals(500, ServerClient.opsTest(ctx).code)
        assertEquals("a test ping is not retried behind the owner's back", 1, hits("/api/ops/test"))
    }
}
