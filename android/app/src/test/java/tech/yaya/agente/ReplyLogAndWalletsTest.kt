package tech.yaya.agente

import android.content.Context
import android.content.pm.PackageInfo
import androidx.test.core.app.ApplicationProvider
import org.json.JSONArray
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.Shadows.shadowOf

@RunWith(RobolectricTestRunner::class)
class ReplyLogAndWalletsTest {
    private lateinit var ctx: Context

    @Before fun setUp() {
        ctx = ApplicationProvider.getApplicationContext()
        for (f in listOf("agente_log", "agente_prefs")) ctx.getSharedPreferences(f, Context.MODE_PRIVATE).edit().clear().commit()
    }

    private fun ev(i: Int) = ReplyEvent(i.toLong(), "com.whatsapp", "WhatsApp", "Ana", "msg $i", i % 2 == 0, "d$i")

    @Test fun theFeedIsNewestFirstAndBounded() {
        var calls = 0
        ReplyLog.listener = { calls++ }
        repeat(105) { ReplyLog.add(ctx, ev(it)) }
        val events = ReplyLog.load(ctx)
        assertEquals(100, events.size)
        assertEquals("msg 104", events.first().incomingText)
        assertEquals("msg 5", events.last().incomingText)
        assertEquals(105, calls)
        ReplyLog.clear(ctx)
        assertTrue(ReplyLog.load(ctx).isEmpty())
        ReplyLog.listener = null
    }

    @Test fun oneBadRecordDoesNotBlankTheOwnersProof() {
        ReplyLog.add(ctx, ev(1))
        val sp = ctx.getSharedPreferences("agente_log", Context.MODE_PRIVATE)
        val arr = JSONArray(sp.getString("events", "[]")).put(JSONObject().put("ts", "not a number"))
        sp.edit().putString("events", arr.toString()).commit()
        assertEquals(listOf("msg 1"), ReplyLog.load(ctx).map { it.incomingText })
        sp.edit().putString("events", "{broken").commit()
        assertTrue(ReplyLog.load(ctx).isEmpty())
    }

    @Test fun theBundledCatalogHasNoDuplicatePackagesAndTheServerCanReplaceIt() {
        val pkgs = Wallets.ALL.map { it.packageName }
        assertEquals(pkgs.size, pkgs.toSet().size)
        assertTrue(Wallets.ALL.all { w -> w.countries.all { it.length == 2 && it == it.uppercase() } })
        assertTrue(Wallets.isKnown(ctx, "com.bcp.innovacxion.yapeapp"))
        val pushed = JSONObject().put("wallets", JSONArray()
            .put(JSONObject().put("package", "pe.new.wallet").put("name", "Nueva").put("countries", JSONArray().put("pe")))
            .put(JSONObject().put("package", " "))
            .put(JSONObject().put("package", "x.noname")))
        val parsed = Wallets.parse(pushed)
        assertEquals(listOf("pe.new.wallet", "x.noname"), parsed.map { it.packageName })
        assertEquals(setOf("PE"), parsed[0].countries)
        assertEquals("x.noname", parsed[1].displayName)
        Prefs.setWalletsJson(ctx, pushed.toString())
        assertTrue(Wallets.isKnown(ctx, "pe.new.wallet"))
        assertTrue("the server list replaces the bundled one", !Wallets.isKnown(ctx, "com.bcp.innovacxion.yapeapp"))
        Prefs.setWalletsJson(ctx, JSONObject().put("wallets", JSONArray()).toString())
        assertTrue("an empty push falls back to the bundled list", Wallets.isKnown(ctx, "com.bcp.innovacxion.yapeapp"))
    }

    @Test fun onboardingOffersTheCountrysWalletsThenInstalledOnesFromHome() {
        val pm = shadowOf(ctx.packageManager)
        for (p in listOf("com.nu.production", "com.venmo", "pe.learned.bank")) pm.installPackage(PackageInfo().apply { packageName = p })
        Prefs.sp(ctx).edit().putStringSet("learned_pay_sources", setOf("pe.learned.bank", "not.installed")).commit()
        val c = Wallets.candidates(ctx, "PE").map { it.packageName }
        assertTrue(c.first() == "com.bcp.innovacxion.yapeapp")
        assertTrue("an installed wallet from another country", "com.venmo" in c)
        assertTrue("installed, learned by the network", "pe.learned.bank" in c)
        assertTrue("uninstalled foreign wallets are not offered", "com.squareup.cash" !in c && "not.installed" !in c)
        assertEquals(c.size, c.toSet().size)
    }
}
