package tech.yaya.agente

import org.json.JSONArray
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner

@RunWith(RobolectricTestRunner::class)
class ExportsAndCreditsTest {
    private fun contacts(vararg c: JSONObject) = JSONArray().apply { c.forEach { put(it) } }

    @Test fun csvCellsCannotBecomeFormulas() {
        val csv = OsSync.csv(contacts(
            JSONObject().put("name", "=HYPERLINK(\"http://evil\",\"x\")").put("phone", "51999888777").put("notes", "@SUM(A1)").put("email", "+cmd@x.pe"),
            JSONObject().put("name", "Ana \"la jefa\"").put("phone", "+51 911").put("messages", 3),
            JSONObject().put("kind", "owner").put("name", "Yo")))
        val rows = csv.trim().split("\n")
        assertEquals("nombre,telefono,email,canal,mensajes,primera_vez,ultima_vez,notas", rows[0])
        assertEquals(3, rows.size)
        assertTrue(rows[1], rows[1].startsWith("\"'=HYPERLINK(\"\"http://evil\"\",\"\"x\"\")\",\"+51999888777\",\"'+cmd@x.pe\""))
        assertTrue(rows[1].endsWith("\"'@SUM(A1)\""))
        assertTrue("quotes are doubled", rows[2].startsWith("\"Ana \"\"la jefa\"\"\",\"+51911\""))
        assertFalse("the owner is never exported", csv.contains("Yo"))
    }

    @Test fun vcardsCannotBeGivenExtraLines() {
        val v = OsSync.vcard(contacts(
            JSONObject().put("name", "Ana\r\nTEL:+1900555").put("phone", "51999").put("email", "a@b.pe\rURL:http://evil").put("notes", "vip; paga, puntual\nsiempre"),
            JSONObject().put("phone", "51888"),
            JSONObject().put("name", "null").put("phone", "null")))
        val lines = v.split("\r\n")
        assertEquals("no injected TEL line", 2, lines.count { it.startsWith("TEL") })
        assertFalse(lines.any { it.startsWith("URL") })
        assertTrue(lines.contains("FN:Ana\\nTEL:+1900555"))
        assertTrue(lines.contains("NOTE:vip\\; paga\\, puntual\\nsiempre"))
        assertTrue("no name: the phone names the card", lines.contains("FN:+51888"))
        assertEquals(2, lines.count { it == "BEGIN:VCARD" })
    }

    @Test fun calendarExportsSkipCancelledAndMarkUnpaidTentative() {
        val ics = OsSync.ics(JSONArray()
            .put(JSONObject().put("id", "a1").put("startsAt", "2026-09-22T10:00").put("durationMins", 30).put("customer", "Ana\r\nX-EVIL:1").put("service", "Corte"))
            .put(JSONObject().put("id", "a2").put("startsAt", "2026-09-22T11:00").put("customer", "Luis").put("status", "pending_payment"))
            .put(JSONObject().put("id", "a3").put("startsAt", "2026-09-22T12:00").put("customer", "Eva").put("status", "cancelled"))
            .put(JSONObject().put("id", "a4").put("startsAt", "mañana").put("customer", "Rota")))
        val lines = ics.split("\r\n")
        assertEquals(2, lines.count { it == "BEGIN:VEVENT" })
        assertFalse(lines.any { it.startsWith("X-EVIL") })
        assertTrue(lines.contains("STATUS:TENTATIVE") && lines.contains("STATUS:CONFIRMED"))
        assertTrue(lines.any { it.startsWith("SUMMARY:Ana\\nX-EVIL:1 · Corte") })
    }

    @Test fun theCreditStateFollowsTheGatewayAndMatchesItsBoundary() {
        fun st(json: String) = Credits.state(JSONObject(json))
        assertEquals(Credits.State.UNKNOWN, Credits.state(null))
        assertEquals("the gateway's word wins", Credits.State.MANUAL, st("""{"state":"manual","balance":50}"""))
        assertEquals(Credits.State.OK, st("""{"balance":2}"""))
        assertEquals(Credits.State.LOW, st("""{"balance":1.99}"""))
        assertEquals(Credits.State.GRACE, st("""{"balance":-0.01}"""))
        assertEquals("exactly the floor is still grace", Credits.State.GRACE, st("""{"balance":-4}"""))
        assertEquals(Credits.State.MANUAL, st("""{"balance":-4.01}"""))
        assertTrue(Credits.manual(JSONObject("""{"balance":-9,"grace":-5}""")))
        assertEquals("$ 8.00", Credits.money(JSONObject(), 8.0))
        assertEquals("S/ 12.50", Credits.money(JSONObject().put("currency", "PEN"), 12.5))
        assertEquals("$ 1.00", Credits.money(JSONObject("""{"currency":null}"""), 1.0))
        assertEquals(0.0, Credits.balance(JSONObject("""{"balance":"NaN"}""")), 0.0)
    }
}
