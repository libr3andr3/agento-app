package tech.yaya.agente

import android.app.Notification
import android.app.PendingIntent
import android.app.RemoteInput
import android.content.Context
import android.content.Intent
import android.os.Process
import android.service.notification.StatusBarNotification
import androidx.test.core.app.ApplicationProvider
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner

@RunWith(RobolectricTestRunner::class)
class LearnedAppsTest {
    private lateinit var ctx: Context
    private val pkg = "com.example.chat"

    @Before fun setUp() {
        ctx = ApplicationProvider.getApplicationContext()
        for (f in listOf("agente_profiles", "agente_observer", "agente_prefs")) ctx.getSharedPreferences(f, Context.MODE_PRIVATE).edit().clear().commit()
    }

    private fun shadow() = NotificationProfile(pkg, "Example", "title_text", 1L, "device", 3).also { ProfileStore.mount(ctx, it) }

    @Test fun aProfileGraduatesAfterTenCleanReadsAndGoesLiveOnlyWithTheOwnersSwitch() {
        shadow()
        repeat(9) { ProfileStore.record(ctx, pkg, true) }
        assertFalse(ProfileStore.get(ctx, pkg)!!.eligible)
        ProfileStore.record(ctx, pkg, true)
        val p = ProfileStore.get(ctx, pkg)!!
        assertTrue(p.eligible)
        assertFalse("eligible is not live: consent stays with the owner", ProfileStore.isLive(ctx, p))
        Prefs.setAppEnabled(ctx, pkg, true)
        assertTrue(ProfileStore.isLive(ctx, p))
        assertNull(ProfileStore.record(ctx, "com.unknown", true))
    }

    @Test fun failingReadsPauseTheProfileAndTurnTheSwitchOff() {
        shadow()
        Prefs.setAppEnabled(ctx, pkg, true)
        repeat(8) { ProfileStore.record(ctx, pkg, true) }
        repeat(2) { assertFalse(ProfileStore.record(ctx, pkg, false)!!.second) } // 2/10: at the line, not over it
        val (p, pausedNow) = ProfileStore.record(ctx, pkg, false)!!                 // 3/11 = 27%
        assertTrue(pausedNow)
        assertTrue(p.paused)
        assertEquals(1, p.unwinds)
        assertFalse("the owner's switch goes off with it", Prefs.isAppEnabled(ctx, pkg))
        assertFalse("a paused profile is not paused twice", ProfileStore.record(ctx, pkg, false)!!.second)
        ProfileStore.resume(ctx, pkg)
        val resumed = ProfileStore.get(ctx, pkg)!!
        assertEquals(Triple(0, 0, false), Triple(resumed.parsedOk, resumed.parseFail, resumed.paused))
        assertEquals("unwinds are remembered across a resume", 1, resumed.unwinds)
    }

    @Test fun aThirdPauseDropsTheProfileAndTheAppSleepsAWeek() {
        shadow()
        repeat(3) {
            repeat(10) { ProfileStore.record(ctx, pkg, false) }
            ProfileStore.resume(ctx, pkg)
        }
        assertNull(ProfileStore.get(ctx, pkg))
        assertTrue(ProfileStore.isAsleep(ctx, pkg))
    }

    @Test fun theRollingWindowLetsOldHistoryFade() {
        shadow()
        repeat(51) { ProfileStore.record(ctx, pkg, true) }
        assertTrue(ProfileStore.get(ctx, pkg)!!.reads <= NotificationProfile.UNWIND_WINDOW)
        assertEquals(listOf(pkg), ProfileStore.all(ctx).map { it.packageName })
    }

    @Suppress("DEPRECATION")
    private fun chat(title: String, text: String, reply: Boolean = true): StatusBarNotification {
        val pi = PendingIntent.getBroadcast(ctx, 0, Intent("x"), PendingIntent.FLAG_IMMUTABLE)
        val b = Notification.Builder(ctx, "c").setSmallIcon(android.R.drawable.ic_dialog_info).setContentTitle(title).setContentText(text)
        if (reply) b.addAction(Notification.Action.Builder(null, "Reply", pi).addRemoteInput(RemoteInput.Builder("r").build()).build())
        return StatusBarNotification(pkg, pkg, 1, null, 0, 0, 0, b.build(), Process.myUserHandle(), 1L)
    }

    @Test fun threeCleanConversationsMountAShadowProfile() {
        assertFalse("no reply action: not a conversation", UnknownAppObserver.observe(ctx, chat("Yape", "S/ 5", reply = false)) { true })
        assertTrue(UnknownAppObserver.observe(ctx, chat("Ana", "hola")) { true })
        assertTrue("a repost is not a new sample", UnknownAppObserver.observe(ctx, chat("Ana", "hola")) { true })
        assertNull(ProfileStore.get(ctx, pkg))
        UnknownAppObserver.observe(ctx, chat("Ana", "¿precio?")) { true }
        UnknownAppObserver.observe(ctx, chat("Luis", "buenas")) { true }
        val p = ProfileStore.get(ctx, pkg)
        assertNotNull(p)
        assertEquals("title_text", p!!.style)
        assertFalse(ProfileStore.isLive(ctx, p))
    }

    @Test fun samplesTheParserCouldNotReadPutTheAppToSleep() {
        UnknownAppObserver.observe(ctx, chat("Ana", "hola")) { true }
        UnknownAppObserver.observe(ctx, chat("Ana", "¿precio?")) { false }
        UnknownAppObserver.observe(ctx, chat("Luis", "buenas")) { true }
        assertNull(ProfileStore.get(ctx, pkg))
        assertTrue(ProfileStore.isAsleep(ctx, pkg))
        assertTrue("a sleeping app is still consumed, never treated as money", UnknownAppObserver.observe(ctx, chat("X", "y")) { true })
    }
}
