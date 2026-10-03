package tech.yaya.agente

import android.content.Context
import androidx.test.core.app.ApplicationProvider
import org.json.JSONArray
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import java.util.Locale

@RunWith(RobolectricTestRunner::class)
class CatalogsTest {
    private lateinit var ctx: Context

    @Before fun setUp() {
        ctx = ApplicationProvider.getApplicationContext()
        Prefs.sp(ctx).edit().clear().commit()
    }

    @Test fun flagsAndNamesComeFromTheIsoCode() {
        val pe = Countries.byIso("PE")
        assertEquals("🇵🇪", pe.flag)
        assertEquals("Peru", pe.name(Locale.ENGLISH))
        assertEquals("Brasil", Countries.byIso("BR").name(Locale("pt")))
        assertEquals("unknown ISO falls back to Perú", "PE", Countries.byIso("ZZ").iso)
        assertEquals("PE", Countries.defaultFor(ctx).iso)
        assertEquals(Countries.ALL.size, Countries.ALL.map { it.iso }.toSet().size)
        assertTrue(Countries.ALL.all { it.dial.all(Char::isDigit) && it.dial.isNotEmpty() })
    }

    @Test @Config(qualifiers = "es") fun searchIgnoresAccentsAndTakesDialCodesAndIso() {
        assertEquals(listOf("PE"), Countries.search(ctx, "peru").map { it.iso })
        assertTrue("MX" in Countries.search(ctx, "MÉXICO").map { it.iso })
        assertTrue(Countries.search(ctx, "+51").any { it.iso == "PE" })
        assertTrue(Countries.search(ctx, "br").any { it.iso == "BR" })
        assertEquals(Countries.all(ctx).size, Countries.search(ctx, "  ").size)
        assertTrue(Countries.search(ctx, "zzzz").isEmpty())
    }

    @Test fun categoriesFallBackToTheBundledListAndKeepProhibitedOnesFlagged() {
        assertTrue(Categories.isProhibited(ctx, "armas"))
        assertFalse(Categories.isProhibited(ctx, "peluqueria"))
        assertFalse(Categories.isProhibited(ctx, "unknown"))
        val pushed = JSONObject()
            .put("categories", JSONArray().put(JSONObject().put("key", "Panaderia").put("es", "Panadería")).put(JSONObject().put("key", " ")))
            .put("prohibited", JSONArray().put(JSONObject().put("key", "tabaco").put("es", "Tabaco").put("en", "Tobacco")))
        val parsed = Categories.parse(pushed)
        assertEquals(listOf("panaderia", "tabaco"), parsed.map { it.key })
        assertEquals("Panadería", parsed[0].pt)
        assertEquals("Tobacco", parsed[1].en)
        Prefs.setCategoriesJson(ctx, pushed.toString())
        assertTrue(Categories.isProhibited(ctx, "tabaco"))
        assertNull("the server list replaces the bundled one", Categories.byKey(ctx, "peluqueria"))
        Prefs.setCategoriesJson(ctx, "{not json")
        assertTrue("a broken cache falls back", Categories.isProhibited(ctx, "armas"))
    }

    @Test fun theOwnersAppIsParsedLenientlyAndBounded() {
        val spec = JSONObject().put("home", "agenda").put("tabs", JSONArray()
            .put(JSONObject().put("id", "hoy").put("label", "Hoy").put("blocks", JSONArray().put("earnings").put("EARNINGS").put("rocket").put(JSONObject().put("type", "attention").put("opts", JSONObject().put("x", 1)))))
            .put(JSONObject().put("label", " ").put("blocks", JSONArray().put("catalog")))
            .put(JSONObject().put("label", "Vacío").put("blocks", JSONArray().put("nope")))
            .put(JSONObject().put("id", "agenda").put("label", "Agenda").put("intro", "null").put("blocks", JSONArray().put("agenda_week")))
            .put(JSONObject().put("label", "C").put("blocks", JSONArray().put("catalog")))
            .put(JSONObject().put("label", "D").put("blocks", JSONArray().put("contacts")))
            .put(JSONObject().put("label", "E").put("blocks", JSONArray().put("earnings"))))
        val ui = UiSpec.parse(spec)!!
        assertEquals(listOf("hoy", "agenda", "tab4", "tab5"), ui.tabs.map { it.id })
        assertEquals(listOf("earnings", "attention"), ui.tabs[0].blocks.map { it.type })
        assertEquals(1, ui.tabs[0].blocks[1].opts!!.getInt("x"))
        assertNull("\"null\" is no intro", ui.tabs[1].intro)
        assertEquals(listOf("money", "agenda", "catalog", "chats"), ui.tabs.map { it.icon })
        assertEquals(1, ui.homeIndex())
        assertNull(UiSpec.parse(JSONObject().put("tabs", JSONArray().put(JSONObject().put("label", "x").put("blocks", JSONArray().put("nope"))))))
        assertNull(UiSpec.parse(null))
        val other = UiSpec.parse(JSONObject(spec.toString()).put("home", "missing"))!!
        assertEquals("an unknown home falls back to the first tab", 0, other.homeIndex())
        assertEquals(ui.signature(), other.signature())
        assertNotEquals(ui.signature(), UiSpec.fallback(ctx, "products").signature())
        assertEquals(listOf(2, 3, 4), listOf("services", "products", null).map { UiSpec.fallback(ctx, it).tabs.size })
    }
}
