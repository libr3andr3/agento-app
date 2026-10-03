package tech.yaya.agente

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

class PhonesTest {
    private val pe = Countries.byIso("PE")
    private val br = Countries.byIso("BR")
    private val us = Countries.byIso("US")

    @Test fun whatPeopleTypeLocally() {
        assertEquals("+51987654321", Phones.e164(pe, "987 654 321"))
        assertEquals("+51987654321", Phones.e164(pe, "0987-654-321"))
        assertEquals("+14155550100", Phones.e164(us, "(415) 555-0100"))
    }

    @Test fun aPastedInternationalNumberIsNotPrefixedTwice() {
        assertEquals("+51987654321", Phones.e164(pe, "+51 987 654 321"))
        assertEquals("+51987654321", Phones.e164(pe, "0051987654321"))
        assertEquals("the dial code typed again without +", "+51987654321", Phones.e164(pe, "51987654321"))
        assertEquals("+14155550100", Phones.e164(us, "1 415 555 0100"))
        assertEquals("a foreign number keeps its own country", "+5215512345678", Phones.e164(pe, "+52 1 55 1234 5678"))
    }

    @Test fun aLocalNumberThatStartsWithTheDialCodeKeepsIt() {
        assertEquals("area code 55 in Brazil", "+555599123456", Phones.e164(br, "55 99123 456"))
        assertEquals("+555599123 4567".replace(" ", ""), Phones.e164(br, "55 99123 4567"))
        assertEquals("+5555991234567", Phones.e164(br, "+55 55 99123 4567"))
    }

    @Test fun tooShortOrTooLongIsNoPhone() {
        assertNull(Phones.e164(pe, "12345"))
        assertNull(Phones.e164(pe, ""))
        assertNull(Phones.e164(pe, "+1234567890123456"))
        assertNull(Phones.e164(pe, "+51 12"))
    }

    @Test fun thePreviewShowsTheNumberTheCodeGoesTo() {
        assertEquals("+51 987 654 321", CountryPicker.pretty(pe, "+51987654321"))
        assertEquals("a pasted foreign number is not shown under +51", "+521 551 234 567 8", CountryPicker.pretty(pe, "+5215512345678"))
    }
}
