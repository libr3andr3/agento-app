package tech.yaya.agente

import android.content.Context
import androidx.test.core.app.ApplicationProvider
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner

@RunWith(RobolectricTestRunner::class)
class InboxQueueTest {
    private lateinit var ctx: Context

    @Before fun setUp() { InboxQueue.resetForTest(); ctx = ApplicationProvider.getApplicationContext() }
    @After fun tearDown() = InboxQueue.resetForTest()

    private fun item(text: String, at: Long, handle: String = "51999888777") =
        InboxQueue.Item(channel = "whatsapp", handle = handle, displayName = "Ana", appPackage = "com.whatsapp", text = text, sentAt = at, isGroup = false, notifKey = "k")

    @Test fun aRepostedBundleOnlyYieldsWhatIsNew() {
        val first = InboxQueue.addNew(ctx, listOf(item("hola", 1), item("quiero 2 pollos", 2)))
        assertEquals(listOf("hola", "quiero 2 pollos"), first.map { it.text })
        assertTrue(first.all { it.id > 0 })
        // The app re-posts the same notification with one more message.
        val again = InboxQueue.addNew(ctx, listOf(item("hola", 1), item("quiero 2 pollos", 2), item("Av. Lima 1", 3)))
        assertEquals(listOf("Av. Lima 1"), again.map { it.text })
        assertTrue("same words, another time, is a new message", InboxQueue.addNew(ctx, listOf(item("hola", 99))).isNotEmpty())
        assertTrue("same time, same words, another person, is a new message", InboxQueue.addNew(ctx, listOf(item("hola", 1, handle = "x"))).isNotEmpty())
    }

    @Test fun strandedMessagesRideAlongWithTheNextTurnUntilSettled() {
        val stranded = InboxQueue.addNew(ctx, listOf(item("¿tienen delivery?", 10)))
        val fresh = InboxQueue.addNew(ctx, listOf(item("¿hola?", 20)))
        val pending = InboxQueue.pendingFor(ctx, "whatsapp", "51999888777", excluding = fresh.map { it.id })
        assertEquals(listOf("¿tienen delivery?"), pending.map { it.text })
        assertEquals(listOf("¿tienen delivery?", "¿hola?"), InboxQueue.pendingFor(ctx, "whatsapp", "51999888777").map { it.text })
        InboxQueue.settle(ctx, (stranded + fresh).map { it.id }, InboxQueue.REPLIED, "¡Sí! ¿A qué dirección?")
        assertTrue(InboxQueue.pendingFor(ctx, "whatsapp", "51999888777").isEmpty())
        assertTrue("another customer's messages never ride along", InboxQueue.pendingFor(ctx, "whatsapp", "other").isEmpty())
        InboxQueue.settle(ctx, emptyList(), InboxQueue.FAILED)
    }

    @Test fun theQueueSurvivesAProcessDeath() {
        InboxQueue.addNew(ctx, listOf(item("hola", 1)))
        InboxQueue.resetForTest()  // a new process opens the same file
        assertTrue(InboxQueue.addNew(ctx, listOf(item("hola", 1))).isEmpty())
        assertEquals(1, InboxQueue.pendingFor(ctx, "whatsapp", "51999888777").size)
    }

    @Test fun pendingIsBoundedAndOldestFirst() {
        InboxQueue.addNew(ctx, (30 downTo 1).map { item("m$it", it.toLong()) })
        val p = InboxQueue.pendingFor(ctx, "whatsapp", "51999888777", limit = 5)
        assertEquals(listOf("m1", "m2", "m3", "m4", "m5"), p.map { it.text })
    }
}
