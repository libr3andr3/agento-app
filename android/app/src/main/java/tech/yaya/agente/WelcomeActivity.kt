package tech.yaya.agente

import android.content.Intent
import android.os.Bundle
import androidx.appcompat.app.AppCompatActivity

/**
 * Launcher: a pure router, no UI (see BOUNDARIES.md flow contract).
 *  - no Yaya account            → AccountActivity (sign in / create; no guest mode)
 *  - not registered             → RegistrationActivity (full-screen step flow)
 *  - registered, no interview   → OnboardingActivity (chat)
 *  - interviewed, apps not set  → ReadAppsActivity (which apps the agent reads, then answers on)
 *  - otherwise                  → DashboardActivity
 *
 * A no-login guest chat as the cold-start entry exists on
 * `consolidate/orphans` and is deliberately NOT wired here: D18 §2 makes the
 * verified WhatsApp number the front door ("no hay puerta de invitado").
 * Its ServerClient.guestChat() was a canned-reply stub — there is no
 * anonymous chat endpoint on the server — and the audit that came with it
 * left two open holes: the guest path creates a business with no account
 * identity, and the activity leaks at the back-stack bottom after
 * registration. Revisit as a product decision, not as a merge resolution.
 *
 * "Interviewed" is inferred from the persisted chat transcript until Prefs
 * grows an explicit onboarded flag; DashboardActivity keeps the chat reachable
 * either way, so the heuristic can never strand anyone. The apps step is
 * skipped for installs that predate it (they configured apps in Settings).
 */
class WelcomeActivity : AppCompatActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val next = when {
            !Prefs.hasIdentity(this) -> AccountActivity::class.java
            !Prefs.serverConfigured(this) -> RegistrationActivity::class.java
            Prefs.chatTranscript(this).isNullOrBlank() -> OnboardingActivity::class.java
            Prefs.appsSetupPending(this) -> ReadAppsActivity::class.java
            else -> DashboardActivity::class.java
        }
        startActivity(Intent(this, next))
        overridePendingTransition(android.R.anim.fade_in, android.R.anim.fade_out)
        finish()
    }
}
