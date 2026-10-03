package tech.yaya.agente

import android.content.Intent
import android.os.Bundle
import android.os.CountDownTimer
import android.text.Editable
import android.text.TextWatcher
import android.view.View
import android.widget.TextView
import android.widget.Toast
import androidx.appcompat.app.AppCompatActivity
import com.google.android.material.button.MaterialButton
import com.google.android.material.card.MaterialCardView
import com.google.android.material.progressindicator.LinearProgressIndicator
import com.google.android.material.textfield.TextInputEditText
import com.google.android.material.textfield.TextInputLayout

/**
 * The agente account — the door to both editions, without passwords. Name
 * and WhatsApp number; "Continuar con agente" sends one code by WhatsApp;
 * the code signs the person in (creating the account the first time) and
 * the core links this phone's agent to it. Opened with [EXTRA_MANAGE] it
 * shows who is signed in and lets them sign out.
 *
 * D18: there is no guest door any more — everyone verifies a number. Play
 * review uses the hardwired OTP_REVIEW_PHONE / OTP_REVIEW_CODE credentials,
 * which is what guest mode was for.
 */
class AccountActivity : AppCompatActivity() {

    companion object {
        const val EXTRA_MANAGE = "manage"
        private const val RESEND_SECONDS = 60
    }

    private lateinit var title: TextView
    private lateinit var sub: TextView
    private lateinit var form: View
    private lateinit var codeForm: View
    private lateinit var nameTil: TextInputLayout
    private lateinit var name: TextInputEditText
    private lateinit var phoneTil: TextInputLayout
    private lateinit var phone: TextInputEditText
    private lateinit var countryCard: MaterialCardView
    private lateinit var countryFlag: TextView
    private lateinit var countryDial: TextView
    private var country: Country = Countries.DEFAULT
    private lateinit var error: TextView
    private lateinit var primary: MaterialButton
    private lateinit var codeSent: TextView
    private lateinit var codeTil: TextInputLayout
    private lateinit var code: TextInputEditText
    private lateinit var codeCta: MaterialButton
    private lateinit var resend: MaterialButton
    private lateinit var progress: LinearProgressIndicator
    private lateinit var restoreCard: View
    private lateinit var signedCard: View
    private var timer: CountDownTimer? = null
    private var busy = false
    /** Bumped on every step change so a stale response can't act. */
    private var seq = 0

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(R.layout.activity_account)
        title = findViewById(R.id.account_title)
        sub = findViewById(R.id.account_sub)
        form = findViewById(R.id.account_form)
        codeForm = findViewById(R.id.account_code_form)
        nameTil = findViewById(R.id.account_name_til)
        name = findViewById(R.id.account_name)
        phoneTil = findViewById(R.id.account_phone_til)
        phone = findViewById(R.id.account_phone)
        countryCard = findViewById(R.id.account_country_card)
        countryFlag = findViewById(R.id.account_country_flag)
        countryDial = findViewById(R.id.account_country_dial)
        error = findViewById(R.id.account_error)
        primary = findViewById(R.id.account_primary)
        codeSent = findViewById(R.id.account_code_sent)
        codeTil = findViewById(R.id.account_code_til)
        code = findViewById(R.id.account_code)
        codeCta = findViewById(R.id.account_code_cta)
        resend = findViewById(R.id.account_resend)
        progress = findViewById(R.id.account_progress)
        restoreCard = findViewById(R.id.account_restore_card)
        signedCard = findViewById(R.id.account_signed_card)

        findViewById<TextView>(R.id.account_welcome_credits).text =
            getString(R.string.account_welcome_credits, Credits.money(null, Credits.WELCOME))
        primary.setOnClickListener { sendCode() }
        codeCta.setOnClickListener { checkCode() }
        resend.setOnClickListener { sendCode(resend = true) }
        findViewById<View>(R.id.account_change).setOnClickListener { showForm() }
        findViewById<View>(R.id.account_restore).setOnClickListener { restore() }
        findViewById<View>(R.id.account_fresh).setOnClickListener { goHome() }
        findViewById<View>(R.id.account_continue).setOnClickListener { goHome() }
        findViewById<View>(R.id.account_logout).setOnClickListener { logout() }
        code.addTextChangedListener(object : TextWatcher {
            override fun beforeTextChanged(s: CharSequence?, a: Int, b: Int, c: Int) {}
            override fun onTextChanged(s: CharSequence?, a: Int, b: Int, c: Int) {}
            override fun afterTextChanged(s: Editable?) {
                codeTil.error = null
                if (s?.length == 6 && !busy) checkCode()
            }
        })
        // The phone's own country (SIM, network, locale): confirm, don't hunt.
        country = Countries.byIso(savedInstanceState?.getString("iso")) .takeIf { savedInstanceState != null } ?: Countries.defaultFor(this)
        updateCountryViews()
        countryCard.setOnClickListener {
            CountryPicker.show(this) { picked -> country = picked; updateCountryViews(); phoneTil.error = null; updatePhonePreview(); phone.requestFocus() }
        }
        phone.addTextChangedListener(object : TextWatcher {
            override fun beforeTextChanged(s: CharSequence?, a: Int, b: Int, c: Int) {}
            override fun onTextChanged(s: CharSequence?, a: Int, b: Int, c: Int) {}
            override fun afterTextChanged(s: Editable?) { phoneTil.error = null; updatePhonePreview() }
        })

        if (intent.getBooleanExtra(EXTRA_MANAGE, false) && Prefs.accountLabel(this).isNotEmpty()) showSigned() else showForm()
    }

    override fun onSaveInstanceState(outState: Bundle) {
        super.onSaveInstanceState(outState)
        outState.putString("iso", country.iso)
    }

    override fun onDestroy() {
        timer?.cancel()
        super.onDestroy()
    }

    private fun updateCountryViews() {
        countryFlag.text = country.flag
        countryDial.text = "+" + country.dial
        countryCard.contentDescription = getString(R.string.reg_country_cd, country.name(this), "+" + country.dial)
    }

    private fun updatePhonePreview() {
        phoneTil.helperText = Phones.e164(country, phone.text?.toString().orEmpty())?.let { CountryPicker.pretty(country, it) }
    }

    private fun showForm() {
        seq++
        timer?.cancel()
        form.visibility = View.VISIBLE
        codeForm.visibility = View.GONE
        signedCard.visibility = View.GONE
        restoreCard.visibility = View.GONE
        error.visibility = View.GONE
        title.setText(R.string.account_title_signin)
        setBusy(false)
    }

    private fun showSigned() {
        form.visibility = View.GONE
        codeForm.visibility = View.GONE
        restoreCard.visibility = View.GONE
        signedCard.visibility = View.VISIBLE
        findViewById<TextView>(R.id.account_signed_as).text = getString(R.string.account_signed_as, Prefs.accountLabel(this))
    }

    private fun setBusy(b: Boolean) {
        busy = b
        progress.visibility = if (b) View.VISIBLE else View.INVISIBLE
        primary.isEnabled = !b
        codeCta.isEnabled = !b
        code.isEnabled = !b
    }

    private fun showError(msg: String) {
        error.text = msg
        error.visibility = View.VISIBLE
    }

    /** E.164 with '+', or "" when the local part is empty. */
    private fun currentPhone(): String = Phones.e164(country, phone.text?.toString().orEmpty()) ?: ""
    private fun currentName() = name.text?.toString()?.trim().orEmpty().ifEmpty { null }

    private fun sendCode(resend: Boolean = false) {
        val p = currentPhone()
        phoneTil.error = null; error.visibility = View.GONE
        if (p.isEmpty()) { phoneTil.error = getString(R.string.account_error_phone); return }
        setBusy(true)
        val my = ++seq
        ServerClient.IO_EXECUTOR.execute {
            // Email is not asked here: the account is the WhatsApp number. A
            // billing email is collected at upgrade time (sales chat, boletas).
            val r = ServerClient.accountOtpStart(this, "", p, currentName())
            runOnUiThread {
                if (my != seq || isFinishing) return@runOnUiThread
                setBusy(false)
                val j = r.json
                if (r.code in 200..299 && j != null) {
                    val sent = j.optJSONObject("sent")
                    val via = when {
                        sent?.optBoolean("whatsapp") == true && sent.optBoolean("email") -> getString(R.string.account_code_sent_both)
                        sent?.optBoolean("whatsapp") == true -> getString(R.string.account_code_sent_whatsapp)
                        else -> getString(R.string.account_code_sent_email)
                    }
                    codeSent.text = getString(R.string.account_code_sent, via)
                    form.visibility = View.GONE
                    codeForm.visibility = View.VISIBLE
                    code.setText("")
                    code.requestFocus()
                    startCountdown()
                } else showError(when (ServerClient.classify(r)) {
                    ServerClient.Kind.OFFLINE -> getString(R.string.account_error_offline)
                    ServerClient.Kind.RATE_LIMITED -> getString(R.string.account_error_rate)
                    ServerClient.Kind.UNAVAILABLE -> getString(R.string.account_error_delivery)
                    else -> getString(R.string.account_error_generic, j?.optString("error").orEmpty().ifEmpty { r.code.toString() })
                })
            }
        }
    }

    private fun startCountdown() {
        timer?.cancel()
        resend.isEnabled = false
        timer = object : CountDownTimer(RESEND_SECONDS * 1000L, 1000L) {
            override fun onTick(ms: Long) { resend.text = getString(R.string.account_resend_in, (ms / 1000L).toInt() + 1) }
            override fun onFinish() { resend.isEnabled = true; resend.setText(R.string.account_resend) }
        }.start()
    }

    private fun checkCode() {
        val c = code.text?.toString()?.trim().orEmpty()
        if (c.length != 6) { codeTil.error = getString(R.string.account_code_hint); return }
        setBusy(true)
        val my = ++seq
        ServerClient.IO_EXECUTOR.execute {
            val r = ServerClient.accountOtpCheck(this, "", currentPhone(), c, currentName())
            runOnUiThread {
                if (my != seq || isFinishing) return@runOnUiThread
                setBusy(false)
                val j = r.json
                if (r.code in 200..299 && j?.optBoolean("signedIn") == true) {
                    Prefs.setAccountEmail(this, j.optString("email").takeIf { it != "null" }.orEmpty())
                    Prefs.setAccountPhone(this, j.optString("phone").takeIf { it != "null" }.orEmpty())
                    afterSignIn()
                } else {
                    codeTil.error = when {
                        r.code == 401 -> getString(R.string.account_error_wrong)
                        r.code == 410 -> getString(R.string.account_error_expired)
                        ServerClient.classify(r) == ServerClient.Kind.OFFLINE -> getString(R.string.account_error_offline)
                        else -> getString(R.string.account_error_generic, j?.optString("error").orEmpty().ifEmpty { r.code.toString() })
                    }
                }
            }
        }
    }

    /** A business phone with nothing on it yet may come from a backup. */
    private fun afterSignIn() {
        timer?.cancel()
        if (Prefs.serverConfigured(this)) { goHome(); return }
        form.visibility = View.GONE
        codeForm.visibility = View.GONE
        restoreCard.visibility = View.VISIBLE
    }

    private fun restore() {
        setBusy(true)
        ServerClient.IO_EXECUTOR.execute {
            val r = ServerClient.restore(this, force = false)
            runOnUiThread {
                setBusy(false)
                val j = r.json
                if (r.code in 200..299 && j != null && j.optString("deviceToken").isNotEmpty()) {
                    Prefs.setDeviceToken(this, j.optString("deviceToken"))
                    Prefs.setBusinessId(this, j.optString("businessId"))
                    Prefs.setLocale(this, j.optJSONObject("locale"))
                    Prefs.setChatTranscript(this, getString(R.string.account_restored))
                    Toast.makeText(this, R.string.account_restored, Toast.LENGTH_LONG).show()
                    goHome()
                } else if (r.code == 404) {
                    Toast.makeText(this, R.string.account_restore_none, Toast.LENGTH_LONG).show()
                    goHome()
                } else showError(getString(R.string.account_error_generic, j?.optString("error").orEmpty().ifEmpty { r.code.toString() }))
            }
        }
    }

    private fun logout() {
        setBusy(true)
        ServerClient.IO_EXECUTOR.execute {
            ServerClient.accountLogout(this)
            runOnUiThread {
                Prefs.setAccountEmail(this, "")
                Prefs.setAccountPhone(this, "")
                showForm()
            }
        }
    }

    /** Where the app goes once a person is signed in: the edition's flow. */
    private fun goHome() {
        val next = if (Prefs.serverConfigured(this)) Screens.HOME else Screens.FIRST_RUN
        startActivity(Intent(this, next).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TASK))
        finish()
    }
}
