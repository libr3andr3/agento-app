package tech.yaya.agente

import android.app.Notification
import android.content.Context
import android.os.Process
import android.service.notification.NotificationListenerService
import android.service.notification.StatusBarNotification
import androidx.test.core.app.ApplicationProvider
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner

@RunWith(RobolectricTestRunner::class)
class PaymentDetectorTest {
    private lateinit var ctx: Context
    private val listener = object : NotificationListenerService() {}
    private val yape = "com.bcp.innovacxion.yapeapp"

    @Before fun setUp() {
        ctx = ApplicationProvider.getApplicationContext()
        Prefs.sp(ctx).edit().clear().commit()
    }

    @Suppress("DEPRECATION")
    private fun sbn(pkg: String, title: String = "Yape", text: String = "Juan te yapeó S/ 25.00", flags: Int = 0, style: Notification.Style? = null): StatusBarNotification {
        val b = Notification.Builder(ctx, "c").setContentTitle(title).setContentText(text).setSmallIcon(android.R.drawable.ic_dialog_info)
        style?.let { b.setStyle(it) }
        val n = b.build().apply { this.flags = this.flags or flags }
        return StatusBarNotification(pkg, pkg, 1, null, 0, 0, 0, n, Process.myUserHandle(), 1_700_000_000_000L)
    }

    @Test fun aWalletNoticeIsForwardedRawWithItsEnvelope() {
        val hit = PaymentDetector.inspect(ctx, listener, sbn(yape))
        assertNotNull(hit)
        assertEquals("Juan te yapeó S/ 25.00", hit!!.text)
        assertEquals(yape, hit.envelope.getString("package"))
        assertEquals(1_700_000_000_000L, hit.envelope.getLong("postTime"))
    }

    @Test fun chatAppsOurOwnAppSummariesAndOngoingAreSkipped() {
        assertNull(PaymentDetector.inspect(ctx, listener, sbn("com.whatsapp")))
        assertNull(PaymentDetector.inspect(ctx, listener, sbn(ctx.packageName)))
        assertNull(PaymentDetector.inspect(ctx, listener, sbn(yape, flags = Notification.FLAG_GROUP_SUMMARY)))
        assertNull(PaymentDetector.inspect(ctx, listener, sbn(yape, flags = Notification.FLAG_ONGOING_EVENT)))
        assertNull(PaymentDetector.inspect(ctx, listener, sbn(yape, title = "", text = "")))
        val msg = Notification.MessagingStyle(android.app.Person.Builder().setName("me").build()).addMessage("hola", 1L, android.app.Person.Builder().setName("Ana").build())
        assertNull("conversations belong to the chat path", PaymentDetector.inspect(ctx, listener, sbn("com.example.bank", style = msg)))
    }

    @Test fun theOwnersSwitchesAndTheAgentsMutesAreObeyed() {
        Prefs.setMoneyAppEnabled(ctx, yape, false)
        assertNull("a wallet switched off is never read", PaymentDetector.inspect(ctx, listener, sbn(yape)))
        Prefs.setMoneyAppEnabled(ctx, yape, true)
        assertNull("an unknown app is not read on a fresh install", PaymentDetector.inspect(ctx, listener, sbn("com.example.bank")))
        Prefs.setReadOtherSources(ctx, true)
        assertNotNull(PaymentDetector.inspect(ctx, listener, sbn("com.example.bank")))
        Prefs.setReadOtherSources(ctx, false)
        assertNull("unknown apps only while 'other apps' is on", PaymentDetector.inspect(ctx, listener, sbn("com.example.bank")))
        assertNotNull("known wallets still read", PaymentDetector.inspect(ctx, listener, sbn(yape)))
        Prefs.setReadOtherSources(ctx, true)
        Prefs.muteSource(ctx, "com.example.bank", System.currentTimeMillis() + 60_000)
        assertNull(PaymentDetector.inspect(ctx, listener, sbn("com.example.bank")))
        Prefs.sp(ctx).edit().putStringSet("learned_pay_sources", setOf("com.example.bank")).commit()
        assertNotNull("a source the network learned is never muted", PaymentDetector.inspect(ctx, listener, sbn("com.example.bank")))
    }

    @Test fun smsAndMailAreNeverMoneyWhateverTheSwitches() {
        Prefs.setReadOtherSources(ctx, true)
        for (pkg in listOf("com.google.android.gm", "com.microsoft.office.outlook", "com.fsck.k9")) {
            Prefs.setCanRead(ctx, pkg, true)
            assertNull("$pkg: the sender writes that text, not a wallet", PaymentDetector.inspect(ctx, listener, sbn(pkg)))
        }
        assertNotNull("an unknown bank with the switch on still is", PaymentDetector.inspect(ctx, listener, sbn("com.example.bank")))
    }

    @Test fun theReadScreenDecidesWhatIsForwarded() {
        Prefs.setCanRead(ctx, yape, false)
        assertNull("a wallet the owner did not let the agent read", PaymentDetector.inspect(ctx, listener, sbn(yape)))
        Prefs.setReadOtherSources(ctx, false)
        Prefs.setCanRead(ctx, "com.example.bank", true)
        assertNotNull("an app switched on one by one, with 'later' off", PaymentDetector.inspect(ctx, listener, sbn("com.example.bank")))
        assertNull(PaymentDetector.inspect(ctx, listener, sbn("com.example.other")))
    }
}
