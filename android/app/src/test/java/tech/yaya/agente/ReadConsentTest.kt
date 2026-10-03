package tech.yaya.agente

import android.app.Notification
import android.app.PendingIntent
import android.app.RemoteInput
import android.content.Context
import android.content.Intent
import android.content.pm.ActivityInfo
import android.content.pm.ApplicationInfo
import android.content.pm.PackageInfo
import android.content.pm.ResolveInfo
import android.os.Process
import android.service.notification.StatusBarNotification
import androidx.test.core.app.ApplicationProvider
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.Robolectric
import org.robolectric.RobolectricTestRunner
import org.robolectric.Shadows.shadowOf

/** The read screen's switches ([Prefs.canRead]), the list it shows ([PhoneApps]), and the listener obeying both. */
@RunWith(RobolectricTestRunner::class)
class ReadConsentTest {
    private lateinit var ctx: Context
    private val w4b = "com.whatsapp.w4b"
    private val wa = "com.whatsapp"
    private val yape = "com.bcp.innovacxion.yapeapp"
    private val game = "com.example.game"
    private val chatApp = "com.example.chat"

    @Before fun setUp() {
        ctx = ApplicationProvider.getApplicationContext()
        for (f in listOf("agente_profiles", "agente_observer", "agente_prefs")) ctx.getSharedPreferences(f, Context.MODE_PRIVATE).edit().clear().commit()
        InboxQueue.resetForTest()
        ctx.deleteDatabase("agente_inbox.db")
        ReplyLog.clear(ctx)
    }

    @After fun tearDown() = InboxQueue.resetForTest()

    // ------------------------------------------------------------ Prefs.canRead

    @Test fun anAppNeverDecidedOnReadsTheWayEarlierBuildsDid() {
        assertFalse("never our own notifications", Prefs.canRead(ctx, ctx.packageName))
        assertTrue("a chat app is read while the agent answers there", Prefs.canRead(ctx, w4b))
        assertFalse(Prefs.canRead(ctx, wa))
        Prefs.setAppEnabled(ctx, wa, true)
        assertTrue(Prefs.canRead(ctx, wa))
        assertTrue("a wallet unless its money switch is off", Prefs.canRead(ctx, yape))
        Prefs.setMoneyAppEnabled(ctx, yape, false)
        assertFalse(Prefs.canRead(ctx, yape))
        assertFalse("anything else: off on a fresh install", Prefs.canRead(ctx, game))
        Prefs.setReadOtherSources(ctx, true)
        assertTrue("anything else while 'apps installed later' is on", Prefs.canRead(ctx, game))
        Prefs.setReadOtherSources(ctx, false)
        assertFalse(Prefs.canRead(ctx, game))
        ProfileStore.mount(ctx, NotificationProfile(chatApp, "Example", "title_text", 1L, "device", 3))
        assertTrue("a learned app is read: that is how it earns its reply switch", Prefs.canRead(ctx, chatApp))
        assertNull(Prefs.readChoice(ctx, game))
    }

    @Test fun anInstallThatAlreadyHadABusinessKeepsReadingOtherAppsUntilItAnswers() {
        Prefs.migrateReadOtherSources(ctx, hadBusiness = true)
        assertTrue("an upgrade keeps the old default", Prefs.canRead(ctx, game))
        assertFalse("and the read screen still asks", Prefs.readOtherSourcesChosen(ctx))
        Prefs.setReadOtherSources(ctx, false)
        assertFalse(Prefs.canRead(ctx, game))
        Prefs.migrateReadOtherSources(ctx, hadBusiness = true)
        assertFalse("the migration runs once and never overrides an answer", Prefs.canRead(ctx, game))
    }

    @Test fun aFreshInstallStartsWithOtherAppsOff() {
        Prefs.migrateReadOtherSources(ctx, hadBusiness = false)
        Prefs.migrateReadOtherSources(ctx, hadBusiness = true)
        assertFalse("registering later does not reopen the door", Prefs.canRead(ctx, game))
        assertFalse(Prefs.readOtherSourcesChosen(ctx))
    }

    @Test fun theOwnersSwitchWinsOverEveryDefault() {
        assertFalse(Prefs.readOtherSourcesChosen(ctx))
        Prefs.setCanRead(ctx, w4b, false)
        assertTrue(Prefs.isAppEnabled(ctx, w4b))
        assertFalse("reply on, read off: not read", Prefs.canRead(ctx, w4b))
        Prefs.setReadOtherSources(ctx, false)
        Prefs.setCanRead(ctx, mapOf(game to true, yape to false))
        assertTrue(Prefs.canRead(ctx, game))
        assertFalse(Prefs.canRead(ctx, yape))
        assertEquals(true, Prefs.readChoice(ctx, game))
        Prefs.setCanRead(ctx, ctx.packageName, true)
        assertFalse("our own app stays out whatever is stored", Prefs.canRead(ctx, ctx.packageName))
        assertTrue(Prefs.readOtherSourcesChosen(ctx))
    }

    // ------------------------------------------------------------ PhoneApps

    private fun app(pkg: String, label: String, kind: PhoneApps.Kind) = PhoneApps.App(pkg, label, kind)

    @Test fun theScreenSuggestsTheBusinessChatWalletsAndLearnedAppsOnly() {
        val both = listOf(w4b, wa, "com.instagram.android")
        assertTrue(PhoneApps.suggested(app(w4b, "WhatsApp Business", PhoneApps.Kind.CHAT), both))
        assertFalse("a personal WhatsApp next to the Business one", PhoneApps.suggested(app(wa, "WhatsApp", PhoneApps.Kind.CHAT), both))
        assertTrue("WhatsApp is the business number when there is no Business app", PhoneApps.suggested(app(wa, "WhatsApp", PhoneApps.Kind.CHAT), listOf(wa)))
        assertFalse(PhoneApps.suggested(app("com.instagram.android", "Instagram", PhoneApps.Kind.CHAT), both))
        assertTrue(PhoneApps.suggested(app(yape, "Yape", PhoneApps.Kind.MONEY), both))
        assertTrue(PhoneApps.suggested(app(chatApp, "Example", PhoneApps.Kind.LEARNED), both))
        assertFalse(PhoneApps.suggested(app(game, "Game", PhoneApps.Kind.OTHER), both))
        assertNull(PhoneApps.businessChat(listOf(game)))
    }

    @Test fun chatAppsComeFirstThenLearnedThenWalletsThenTheRestByName() {
        val ordered = PhoneApps.order(listOf(
            app("z", "zeta", PhoneApps.Kind.OTHER),
            app(yape, "Yape", PhoneApps.Kind.MONEY),
            app("a", "Álbum", PhoneApps.Kind.OTHER),
            app(wa, "WhatsApp", PhoneApps.Kind.CHAT),
            app(chatApp, "Example", PhoneApps.Kind.LEARNED),
            app("b", "banco", PhoneApps.Kind.MONEY),
        )).map { it.packageName }
        assertEquals(listOf(wa, chatApp, "b", yape, "a", "z"), ordered)
        assertEquals(PhoneApps.Kind.CHAT, PhoneApps.kindOf(wa, setOf(wa), emptySet()))
        assertEquals(PhoneApps.Kind.MONEY, PhoneApps.kindOf(yape, setOf(yape), emptySet()))
        assertEquals(PhoneApps.Kind.OTHER, PhoneApps.kindOf(game, setOf(yape), setOf(chatApp)))
    }

    @Test fun theListIsEveryLauncherAppPlusKnownOnesWithoutAnIconButNeverUs() {
        val pm = shadowOf(ctx.packageManager)
        fun install(pkg: String, label: String, launcher: Boolean) {
            val ai = ApplicationInfo().apply { packageName = pkg; nonLocalizedLabel = label }
            pm.installPackage(PackageInfo().apply { packageName = pkg; applicationInfo = ai })
            if (launcher) pm.addResolveInfoForIntent(
                Intent(Intent.ACTION_MAIN).addCategory(Intent.CATEGORY_LAUNCHER),
                ResolveInfo().apply { activityInfo = ActivityInfo().apply { packageName = pkg; name = "$pkg.Main"; applicationInfo = ai } },
            )
        }
        install(w4b, "WA Business", launcher = true)
        install(game, "Game", launcher = true)
        install(yape, "Yape", launcher = false)
        install("com.example.hidden", "Hidden", launcher = false)
        val list = PhoneApps.list(ctx)
        val pkgs = list.map { it.packageName }
        assertTrue(w4b in pkgs && game in pkgs)
        assertTrue("a known wallet without a launcher entry", yape in pkgs)
        assertFalse("an unknown app without an icon falls under 'apps installed later'", "com.example.hidden" in pkgs)
        assertFalse(ctx.packageName in pkgs)
        assertEquals("chat apps under the names the reply screen uses", "WhatsApp Business", list.first { it.packageName == w4b }.label)
        assertEquals(listOf(PhoneApps.Kind.CHAT, PhoneApps.Kind.MONEY, PhoneApps.Kind.OTHER), list.filter { it.packageName in setOf(w4b, yape, game) }.map { it.kind })
    }

    // ------------------------------------------------------------ the listener

    private val listener by lazy { Robolectric.buildService(AgenteNotificationListener::class.java).create().get() }

    @Suppress("DEPRECATION")
    private fun chat(pkg: String, title: String, text: String, at: Long): StatusBarNotification {
        val pi = PendingIntent.getBroadcast(ctx, 0, Intent("x"), PendingIntent.FLAG_IMMUTABLE)
        val n = Notification.Builder(ctx, "c").setSmallIcon(android.R.drawable.ic_dialog_info)
            .setContentTitle(title).setContentText(text).setWhen(at)
            .addAction(Notification.Action.Builder(null, "Reply", pi).addRemoteInput(RemoteInput.Builder("r").build()).build())
            .build()
        return StatusBarNotification(pkg, pkg, 1, null, 0, 0, 0, n, Process.myUserHandle(), at)
    }

    @Test fun anAppTheOwnerDidNotLetUsReadIsNeverLookedAt() {
        Prefs.setEnabled(ctx, true)
        Prefs.setCanRead(ctx, chatApp, false)
        Prefs.setCanRead(ctx, w4b, false)
        listOf("hola", "¿precio?", "buenas").forEachIndexed { i, t ->
            listener.onNotificationPosted(chat(chatApp, "Ana$i", t, 1_700_000_000_000L + i))
        }
        listener.onNotificationPosted(chat(w4b, "Luis", "hola", 1_700_000_000_000L))
        assertTrue("nothing logged", ReplyLog.load(ctx).isEmpty())
        assertTrue("the observer kept no sample", ctx.getSharedPreferences("agente_observer", Context.MODE_PRIVATE).all.isEmpty())
        assertNull(ProfileStore.get(ctx, chatApp))

        // The same three notifications, once reading is allowed, teach the app.
        Prefs.setCanRead(ctx, chatApp, true)
        listOf("hola", "¿precio?", "buenas").forEachIndexed { i, t ->
            listener.onNotificationPosted(chat(chatApp, "Ana$i", t, 1_700_000_000_000L + i))
        }
        assertNotNull(ProfileStore.get(ctx, chatApp))
    }

    @Test fun aChatReadButNotAnsweredIsRecordedOnceAndNeverReplied() {
        Prefs.setEnabled(ctx, true)
        Prefs.setCanRead(ctx, wa, true)
        Prefs.setAppEnabled(ctx, wa, false)
        listener.onNotificationPosted(chat(wa, "Ana", "hola, ¿abren hoy?", 1_700_000_000_000L))
        val log = ReplyLog.load(ctx)
        assertEquals(1, log.size)
        assertFalse(log[0].replySent)
        assertEquals(ctx.getString(R.string.log_read_only), log[0].detail)
        assertEquals("hola, ¿abren hoy?", log[0].incomingText)
        listener.onNotificationPosted(chat(wa, "Ana", "hola, ¿abren hoy?", 1_700_000_000_000L))
        assertEquals("a repost is not a second message", 1, ReplyLog.load(ctx).size)

        // Replies switched on: the next message is answered (canned mode, no server here).
        Prefs.setAppEnabled(ctx, wa, true)
        listener.onNotificationPosted(chat(wa, "Ana", "¿hay delivery?", 1_700_000_060_000L))
        // Only the new message: the read-only one was settled, so it does not ride along.
        val answered = ReplyLog.load(ctx).filter { it.incomingText == "¿hay delivery?" }
        assertEquals(1, answered.size)
        assertTrue(answered[0].replySent)
    }
}
