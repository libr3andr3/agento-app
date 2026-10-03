package tech.yaya.agente

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class ChannelTest {
    @Test
    fun twoWhatsAppsAreOneChannelButFacebookAndMessengerAreNot() {
        assertEquals(Channel.of("com.whatsapp").id, Channel.of("com.whatsapp.w4b").id)
        assertNotEquals(Channel.of("com.facebook.katana").id, Channel.of("com.facebook.orca").id)
        assertEquals("messenger", Channel.of("com.facebook.mlite").id)
        assertEquals("telegram", Channel.of("org.telegram.plus").id)
        assertEquals("tiktok", Channel.of("com.ss.android.ugc.trill").id)
    }

    @Test
    fun anUnknownAppIsItsOwnChannel() {
        assertEquals(Channel("com.example.chat", "Example"), Channel.of("com.example.chat", "Example"))
        assertEquals(Channel("com.example.chat", "com.example.chat"), Channel.of("com.example.chat"))
    }

    @Test
    fun phoneNamesKeyOnDigitsEverythingElseOnTheNormalisedName() {
        val wa = Channel.of("com.whatsapp")
        val p = SenderRef.from(wa, " +51 999-888 777 ")
        assertTrue(p.isPhone)
        assertEquals("51999888777", p.handle)
        assertEquals("+51 999-888 777", p.displayName)
        val n = SenderRef.from(wa, "  Ana   María ")
        assertFalse(n.isPhone)
        assertEquals("ana maría", n.handle)
        assertEquals(SenderRef.from(wa, "ANA MARÍA").handle, n.handle)
        // Short numbers and mixed text are names, not phones.
        assertFalse(SenderRef.from(wa, "12345").isPhone)
        assertFalse(SenderRef.from(wa, "Pedro 999 888 777").isPhone)
        // Emoji and punctuation are part of a name.
        assertNotEquals(SenderRef.from(wa, "Ana 🌵").handle, SenderRef.from(wa, "Ana").handle)
    }

    @Test
    fun theLegacyPeerIdIsUnchanged() {
        val r = SenderRef.from(Channel.of("com.whatsapp"), "Ana")
        assertEquals("com.whatsapp:Ana", r.legacyPeer("com.whatsapp"))
    }

    @Test
    fun supportedAppsDefaultToTheBusinessInboxOnly() {
        assertEquals(setOf("com.whatsapp.w4b"), SupportedApps.DEFAULT_ENABLED)
        assertTrue(SupportedApps.DEFAULT_ENABLED.all { SupportedApps.isSupported(it) })
        assertEquals("Signal", SupportedApps.get("org.thoughtcrime.securesms")?.displayName)
        assertNull(SupportedApps.get("com.example"))
        // Every supported app maps to a known channel, not a package fallback.
        for (a in SupportedApps.ALL) assertNotEquals(a.packageName, Channel.of(a.packageName).id)
    }
}
