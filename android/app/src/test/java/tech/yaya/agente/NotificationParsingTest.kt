package tech.yaya.agente

import android.app.Notification
import android.app.PendingIntent
import android.app.RemoteInput
import android.content.Context
import android.content.Intent
import androidx.core.app.NotificationCompat
import androidx.core.app.Person
import androidx.test.core.app.ApplicationProvider
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner

@RunWith(RobolectricTestRunner::class)
class NotificationParsingTest {
    private val ctx: Context = ApplicationProvider.getApplicationContext()
    private val listener = AgenteNotificationListener()
    private val me = Person.Builder().setName("Yo").build()
    private val ana = Person.Builder().setName("Ana").build()

    private fun messaging(vararg msgs: Pair<Person?, String>, title: String? = null, group: Boolean = false): Notification {
        val style = NotificationCompat.MessagingStyle(me).setConversationTitle(title).setGroupConversation(group)
        msgs.forEachIndexed { i, (who, text) -> style.addMessage(text, 1_000L * (msgs.size - i), who) }
        return NotificationCompat.Builder(ctx, "c").setSmallIcon(android.R.drawable.ic_dialog_info).setStyle(style).build()
    }

    private fun plain(title: String, text: String, `when`: Long = 5_000L): Notification =
        NotificationCompat.Builder(ctx, "c").setSmallIcon(android.R.drawable.ic_dialog_info).setContentTitle(title).setContentText(text).setWhen(`when`).build()

    @Test fun everyMessageInABundleIsReadOldestFirstAndOurOwnAreDropped() {
        val n = messaging(ana to "mi dirección es Av. Lima 1", null to "gracias!", ana to "quiero 2 pollos", ana to "hola")
        val c = listener.parseConversation(n)!!
        assertEquals("Ana", c.sender)
        assertEquals(listOf("hola", "quiero 2 pollos", "mi dirección es Av. Lima 1"), c.inbound.map { it.text })
        assertFalse(c.isGroup)
        assertEquals("hola\\nquiero 2 pollos\\nmi dirección es Av. Lima 1".replace("\\n", "\n"), c.asMessage().text)
    }

    @Test fun ourOwnEchoIsNotACustomerTurnAndGroupsNameTheRoom() {
        assertTrue(listener.parseConversation(messaging(me to "listo, te esperamos"))!!.inbound.isEmpty())
        val g = listener.parseConversation(messaging(ana to "¿abren hoy?", title = "Vecinos", group = true))!!
        assertEquals("Vecinos", g.sender)
        assertTrue(g.isGroup)
    }

    @Test fun plainNotificationsFallBackToTitleAndText() {
        val c = listener.parseConversation(plain("Ana", "hola, ¿tienen stock?", `when` = 42L))!!
        assertEquals("Ana", c.sender)
        assertEquals(42L, c.inbound.single().sentAt)
        val bucketed = listener.parseConversation(plain("Ana", "hola", `when` = 0L))!!.inbound.single().sentAt
        assertEquals("no timestamp: bucketed to the minute", 0L, bucketed % 60_000L)
    }

    @Test fun systemPlaceholdersAreSkipped() {
        for (t in listOf("3 new messages", "2 new messages from 2 chats", "Checking for new messages", "New message", "5 messages", "10 chats")) {
            assertNull(t, listener.parseConversation(plain("Instagram", t)))
        }
        assertNull(listener.parseConversation(plain("Ana", "   ")))
    }

    @Test fun customersWhoMentionTheseWordsAreStillHeard() {
        for (t in listOf("Hi, checking for availability tomorrow?", "I saw your new message about prices", "¿me mandas 2 messages con la carta?")) {
            assertNotNull(t, listener.parseConversation(plain("Ana", t)))
        }
    }

    @Test fun theReplyActionIsTheOneWithARemoteInput() {
        val pi = PendingIntent.getBroadcast(ctx, 0, Intent("x"), PendingIntent.FLAG_IMMUTABLE)
        val input = RemoteInput.Builder("reply").setLabel("Responder").build()
        val markRead = Notification.Action.Builder(null, "Marcar como leído", pi).build()
        val other = Notification.Action.Builder(null, "Otro", pi).addRemoteInput(input).build()
        val reply = Notification.Action.Builder(null, "Responder", pi).addRemoteInput(input).build()
        fun with(vararg a: Notification.Action) = Notification.Builder(ctx, "c").setSmallIcon(android.R.drawable.ic_dialog_info).setActions(*a).build()
        assertEquals("Responder", listener.findReplyAction(with(markRead, other, reply))!!.title)
        assertEquals("Otro", listener.findReplyAction(with(markRead, other))!!.title)
        assertNull(listener.findReplyAction(with(markRead)))
        val worn = Notification.Builder(ctx, "c").setSmallIcon(android.R.drawable.ic_dialog_info)
            .extend(Notification.WearableExtender().addAction(reply)).build()
        assertEquals("the wearable extender is a fallback", "Responder", listener.findReplyAction(worn)!!.title)
    }

    @Test fun aNullReplyIsSilenceNotTheWordNull() {
        val r = AgenteNotificationListener::replyTextOf
        assertNull(r(null))
        assertNull(r(org.json.JSONObject("{\"agentResponse\": null}")))
        assertNull(r(org.json.JSONObject("{\"agentResponse\": \"  \"}")))
        assertNull(r(org.json.JSONObject("{}")))
        assertEquals("¡Hola!", r(org.json.JSONObject("{\"agentResponse\": \"¡Hola!\"}")))
    }
}
