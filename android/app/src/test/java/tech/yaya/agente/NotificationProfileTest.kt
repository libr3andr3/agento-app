package tech.yaya.agente

import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class NotificationProfileTest {
    private fun p(ok: Int, fail: Int, paused: Long = 0L) =
        NotificationProfile("com.x", "X", "messaging", 1L, "device", 3, parsedOk = ok, parseFail = fail, pausedAt = paused)

    @Test
    fun eligibilityNeedsEnoughCleanReads() {
        assertFalse(p(9, 0).eligible)
        assertTrue(p(10, 0).eligible)
        assertTrue(p(10, 2).eligible, )  // 2/12 = 16.7% < 20%
        assertFalse(p(10, 3).eligible)   // 3/13 = 23%
        assertEquals(0.0, p(0, 0).failRate, 0.0)
        assertEquals(13, p(10, 3).reads)
        assertTrue(p(1, 0, paused = 5L).paused)
    }

    @Test
    fun jsonRoundTripsAndRejectsAProfileWithoutAPackage() {
        val orig = p(7, 1).copy(unwinds = 1, pausedAt = 99L)
        assertEquals(orig, NotificationProfile.fromJson(JSONObject(orig.toJson().toString())))
        assertNull(NotificationProfile.fromJson(JSONObject().put("package", " ")))
        val sparse = NotificationProfile.fromJson(JSONObject().put("package", "com.y"))!!
        assertEquals("com.y", sparse.displayName)
        assertEquals("title_text", sparse.style)
        assertEquals("device", sparse.builtBy)
    }
}
