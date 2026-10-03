package tech.yaya.agente

import android.content.Context
import androidx.test.core.app.ApplicationProvider
import org.json.JSONArray
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner

@RunWith(RobolectricTestRunner::class)
class HelpersTest {
    private val ctx: Context = ApplicationProvider.getApplicationContext()

    @Test fun contactsAreNamedByNameThenPhoneThenWhereTheyCameFrom() {
        assertEquals("Ana", Crm.displayName(ctx, JSONObject().put("name", "Ana").put("phone", "519"), "x"))
        assertEquals("+51999", Crm.displayName(ctx, JSONObject().put("name", "null").put("phone", "51999"), "x"))
        assertEquals(ctx.getString(R.string.crm_source_network), Crm.displayName(ctx, null, "agent:abc"))
        assertEquals("Luis", Crm.displayName(ctx, null, "com.whatsapp:Luis"))
        assertEquals(ctx.getString(R.string.crm_unknown_name), Crm.displayName(ctx, null, "com.whatsapp:"))
        assertEquals("AM", Crm.initials("ana maría pérez"))
        assertEquals("☺", Crm.initials("+51 999"))
        assertEquals("", Crm.planLabel(ctx, null))
        assertEquals(ctx.getString(R.string.crm_plan_max), Crm.planLabel(ctx, JSONObject().put("plan", "enterprise")))
        assertTrue(Crm.metaLine(ctx, JSONObject().put("source", "network").put("email", "a@b.pe")).contains("a@b.pe"))
        assertEquals("", Crm.shortTime("mañana"))
        assertTrue(Crm.shortTime("2020-01-02T03:04:05Z").isNotEmpty())
    }

    @Test fun ordersAndDatesReadNaturally() {
        val o = JSONObject().put("items", JSONArray().put(JSONObject().put("qty", 2).put("product", "Pollo")).put(JSONObject().put("product", "Chicha")))
        assertEquals("2× Pollo, 1× Chicha", Blocks.orderSummary(o))
        assertEquals("", Blocks.orderSummary(JSONObject()))
        assertEquals("not a date: shown as is", "pronto", Blocks.prettyDate("pronto"))
        assertTrue(Blocks.prettyDate("2026-09-22") != "2026-09-22")
    }

    @Test fun theInstallReferrerKeepsItsCampaign() {
        InstallReferrer.store(ctx, "utm_source=facebook&utm_medium=cpc&utm_campaign=barberos%20lima")
        assertEquals(listOf("facebook", "cpc", "barberos lima"), listOf(Prefs.referralSource(ctx), Prefs.referralMedium(ctx), Prefs.referralCampaign(ctx)))
        assertTrue(Prefs.referrerFetched(ctx))
        InstallReferrer.store(ctx, " ")
        assertNull(Prefs.installReferrerRaw(ctx))
        assertNull(Prefs.referralSource(ctx))
    }
}
