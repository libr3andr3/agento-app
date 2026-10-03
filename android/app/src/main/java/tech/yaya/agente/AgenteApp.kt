package tech.yaya.agente

import android.app.Application
import androidx.appcompat.app.AppCompatDelegate

class AgenteApp : Application() {
    override fun onCreate() {
        super.onCreate()
        // Colors are tuned for light mode; budget phones often default to dark,
        // which made green-on-dark unreadable. Force light until a proper
        // dark palette exists.
        AppCompatDelegate.setDefaultNightMode(AppCompatDelegate.MODE_NIGHT_NO)
        OwnerAlerts.ensureChannel(this)
        // Fire-and-forget: reads the Play Store install referrer (campaign
        // attribution) well ahead of registration, which is the first place
        // it's needed. No-op after the first successful read.
        InstallReferrer.fetch(this)
        // One-time decision that must be taken before anything reads it:
        // whether this install keeps the old "read any app's notices" default.
        // It looks at whether a business already exists, so it runs before
        // the core boots.
        Prefs.migrateReadOtherSources(this)
        // Boot the on-device agent early so the first customer message does
        // not pay the startup cost. Safe to call again from any thread.
        Thread({
            AgenteCore.ensureStarted(this)
            // Server-pushed catalogs (money apps, business categories):
            // refreshed at launch when stale, bundled defaults otherwise.
            runCatching { Wallets.refresh(this) }
            runCatching { Categories.refresh(this) }
        }, "agente-core-boot").start()
    }
}
