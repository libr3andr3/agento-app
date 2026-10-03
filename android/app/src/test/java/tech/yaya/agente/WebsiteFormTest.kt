package tech.yaya.agente

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner

@RunWith(RobolectricTestRunner::class)
class WebsiteFormTest {
    @Test fun theDomainIsWhatTheOwnerMeant() {
        assertEquals("example.com", WebsiteForm.domainOf("example.com"))
        assertEquals("www.example.com", WebsiteForm.domainOf("  https://www.Example.com/perfil?x=1 "))
        assertEquals("api.example.org", WebsiteForm.domainOf("http://api.example.org:8443/"))
        assertEquals("", WebsiteForm.domainOf("   "))
    }

    @Test fun theDefaultLinkFollowsTheDomain() {
        assertEquals("https://example.com/perfil/{id}", WebsiteForm.defaultTemplate("example.com"))
        assertEquals("", WebsiteForm.defaultTemplate(""))
    }

    @Test fun thePreviewMatchesTheCoresExample() {
        assertEquals("https://example.com/perfil/p7k2m9x4qa", WebsiteForm.preview("https://example.com/perfil/{id}"))
        assertEquals("https://m.pe/productor/rosa-quispe-51987654321", WebsiteForm.preview("https://m.pe/{role}/{slug}-{phone}"))
    }

    @Test fun theBodyCarriesEveryFieldTrimmed() {
        val b = WebsiteForm.body(" https://Example.com/ ", "  ", " https://api.example.com/ev ", false)
        assertEquals("example.com", b.getString("domain"))
        assertEquals("", b.getString("profileUrl"))
        assertEquals("https://api.example.com/ev", b.getString("webhookUrl"))
        assertFalse(b.getBoolean("sendProfileLink"))
        assertTrue(WebsiteForm.body("m.pe", "", "", true).getBoolean("sendProfileLink"))
    }
}
