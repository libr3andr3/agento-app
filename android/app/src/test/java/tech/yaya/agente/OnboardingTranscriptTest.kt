package tech.yaya.agente

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import tech.yaya.agente.OnboardingTranscript.AGENT
import tech.yaya.agente.OnboardingTranscript.OWNER
import tech.yaya.agente.OnboardingTranscript.SYSTEM

class OnboardingTranscriptTest {
    private val typing = "…"

    @Test fun aReplyWithParagraphsComesBackAsOneBubble() {
        val chat = listOf(
            OWNER to "hola",
            AGENT to "¡Hola! Soy tu agente.\n\nPrimero, ¿cómo se llama tu negocio?\n\n1. Nombre\n2. Rubro",
            SYSTEM to "✓ Registrado",
        )
        assertEquals(chat, OnboardingTranscript.decode(OnboardingTranscript.encode(chat), typing))
    }

    @Test fun oldTranscriptsStillLoadAsBefore() {
        val legacy = "🧑 hola\n\n🟢 ¡Hola!\n\n✓ Registrado\n\n…"
        assertEquals(listOf(OWNER to "hola", AGENT to "¡Hola!", SYSTEM to "✓ Registrado"), OnboardingTranscript.decode(legacy, typing))
    }

    @Test fun anEmptyChatIsBlankForTheLauncher() {
        assertEquals("", OnboardingTranscript.encode(emptyList()))
        assertTrue(OnboardingTranscript.decode(null, typing).isEmpty())
        assertTrue(OnboardingTranscript.decode("  ", typing).isEmpty())
        assertEquals("a broken JSON-looking legacy line is still shown", listOf(SYSTEM to "[nota]"), OnboardingTranscript.decode("[nota]", typing))
        assertEquals(listOf(SYSTEM to "x"), OnboardingTranscript.decode("""[{"role":"weird","text":"x"},{"role":"agent","text":"…"}]""", typing))
    }
}
