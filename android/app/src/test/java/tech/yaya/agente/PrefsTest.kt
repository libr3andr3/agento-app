package tech.yaya.agente

import android.content.Context
import androidx.test.core.app.ApplicationProvider
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner

@RunWith(RobolectricTestRunner::class)
class PrefsTest {
    private lateinit var ctx: Context

    @Before fun setUp() {
        ctx = ApplicationProvider.getApplicationContext()
        Prefs.sp(ctx).edit().clear().commit()
    }

    @Test fun freshInstallDefaultsAreTheSafeOnes() {
        assertFalse(Prefs.isEnabled(ctx))
        assertTrue("WhatsApp Business only, out of the box", Prefs.isAppEnabled(ctx, "com.whatsapp.w4b"))
        assertFalse(Prefs.isAppEnabled(ctx, "com.whatsapp"))
        assertFalse(Prefs.replyToGroups(ctx))
        assertTrue(Prefs.clearAfterReply(ctx))
        assertEquals(30, Prefs.cooldownMinutes(ctx))
        assertFalse(Prefs.serverConfigured(ctx))
        assertEquals("PE", Prefs.country(ctx))
        assertEquals(BuildConfig.SUPPORT_WHATSAPP, Prefs.supportPhone(ctx))
    }

    @Test fun aBlankCannedReplyIsNeverSent() {
        Prefs.setReplyText(ctx, "   ")
        assertEquals(ctx.getString(R.string.default_reply), Prefs.replyText(ctx))
        Prefs.setReplyText(ctx, "Ya te respondemos")
        assertEquals("Ya te respondemos", Prefs.replyText(ctx))
        Prefs.setCooldownMinutes(ctx, -5)
        assertEquals(0, Prefs.cooldownMinutes(ctx))
    }

    @Test fun moneyReadsTheWayTheBusinessCounts() {
        assertEquals("S/ 50", Prefs.money(ctx, 50.0))
        assertEquals("S/ 12.50", Prefs.money(ctx, 12.5))
        Prefs.setLocale(ctx, JSONObject().put("country", "IN").put("currency", "INR").put("currencySymbol", "₹"))
        assertEquals("₹ 1200.50", Prefs.money(ctx, 1200.5))
        Prefs.setLocale(ctx, JSONObject().put("country", "XX").put("currency", "XXX").put("currencySymbol", ""))
        assertEquals("an unknown symbol shows the code", "50 XXX", Prefs.money(ctx, 50.0))
        Prefs.setLocale(ctx, JSONObject("{\"country\": \"CL\", \"currency\": \"CLP\", \"currencySymbol\": null}"))
        assertFalse("a JSON null is not the symbol \"null\"", Prefs.money(ctx, 5.0).contains("null"))
        Prefs.setLocale(ctx, null, fallbackCountry = "MX")
        assertEquals("MX", Prefs.country(ctx))
    }

    @Test fun theSupportLineIsRememberedOnlyWhenItIsAPhone() {
        Prefs.rememberSupport(ctx, JSONObject().put("support", JSONObject().put("phone", "null")))
        assertEquals(BuildConfig.SUPPORT_WHATSAPP, Prefs.supportPhone(ctx))
        Prefs.setSupportPhone(ctx, "123")
        assertEquals(BuildConfig.SUPPORT_WHATSAPP, Prefs.supportPhone(ctx))
        Prefs.rememberSupport(ctx, JSONObject().put("support", JSONObject().put("phone", "+51 999 111 222")))
        assertEquals("51999111222", Prefs.supportPhone(ctx))
    }

    @Test fun theAccountLabelPrefersTheEmailThenThePhone() {
        assertFalse(Prefs.hasIdentity(ctx))
        Prefs.setAccountPhone(ctx, "51999111222")
        assertTrue(Prefs.hasIdentity(ctx))
        assertEquals("+51999111222", Prefs.accountLabel(ctx))
        Prefs.setAccountEmail(ctx, "a@b.pe")
        assertEquals("a@b.pe", Prefs.accountLabel(ctx))
        assertFalse(Prefs.isSourceMuted(ctx, "x"))
        Prefs.muteSource(ctx, "x", System.currentTimeMillis() - 1)
        assertFalse("a lapsed mute is no mute", Prefs.isSourceMuted(ctx, "x"))
        assertTrue(Prefs.learnedSourcesStale(ctx))
        assertNotEquals("", Prefs.serverUrl(ctx))
    }
}
