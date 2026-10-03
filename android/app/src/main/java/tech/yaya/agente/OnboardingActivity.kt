package tech.yaya.agente

import android.Manifest
import android.animation.ObjectAnimator
import android.content.ClipData
import android.content.ComponentName
import android.content.Intent
import android.content.pm.PackageManager
import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.graphics.Matrix
import android.media.MediaPlayer
import android.media.MediaRecorder
import android.net.Uri
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.provider.MediaStore
import android.provider.Settings
import android.util.Base64
import android.view.LayoutInflater
import android.view.View
import android.view.ViewGroup
import android.widget.LinearLayout
import android.widget.TextView
import android.widget.Toast
import androidx.appcompat.app.AppCompatActivity
import androidx.core.app.ActivityCompat
import androidx.core.content.ContextCompat
import androidx.core.content.FileProvider
import androidx.exifinterface.media.ExifInterface
import androidx.recyclerview.widget.LinearLayoutManager
import androidx.recyclerview.widget.RecyclerView
import com.google.android.material.button.MaterialButton
import com.google.android.material.dialog.MaterialAlertDialogBuilder
import com.google.android.material.shape.MaterialShapeDrawable
import com.google.android.material.shape.ShapeAppearanceModel
import com.google.android.material.textfield.TextInputEditText
import java.io.ByteArrayInputStream
import java.io.ByteArrayOutputStream
import java.io.File

/**
 * The owner's chat with their business — first the setup interview, later the
 * management console (the server decides; the surface is the same). Real
 * bubbles in a RecyclerView, a typing indicator while the server thinks, and
 * the "¡Todo listo!" sheet which gates on Notification Access so the agent can
 * never finish setup dead. Registration/OTP live in RegistrationActivity.
 */
class OnboardingActivity : AppCompatActivity() {

    companion object {
        private const val RC_CATALOG_CAMERA = 72
        private const val RC_CATALOG_GALLERY = 73
        private const val RC_VOICE_LISTEN = 74
        /** Longest edge for uploaded catalog photos. */
        private const val MAX_PHOTO_EDGE = 1600
        /** Transcript role markers — also the on-disk serialization prefixes. */
        private const val OWNER_PREFIX = "🧑 "
        private const val AGENT_PREFIX = "🟢 "
        /** Chat-UI prefs — OUR file, never Prefs.kt (see BOUNDARIES.md). */
        private const val UI_PREFS = "agente_chat_ui"
        private const val KEY_VOICE_MODE = "voice_mode_on"
        /** Beat before auto-listening when a reply arrived without TTS audio. */
        private const val LISTEN_AFTER_TEXT_MS = 1100L

        // --- WhatsApp-style hold-to-record gesture ---
        /** A press shorter than this is a tap, not a recording: discard + hint. */
        private const val MIN_HOLD_MS = 400L
        /** Drag left this far (dp) while holding → cancel and discard. */
        private const val CANCEL_DRAG_DP = 90f
        /** Drag up this far (dp) while holding → lock hands-free recording. */
        private const val LOCK_DRAG_DP = 70f
        /** Longer beat on open — the owner is still reading the seeded greeting. */
        private const val LISTEN_ON_OPEN_MS = 1600L

        // --- DIY voice-activity detection over MediaRecorder.getMaxAmplitude()
        //     (budget phones often lack on-device Spanish models, so hands-free
        //     capture always transcribes through the server's Whisper path) ---
        /** Amplitude poll cadence. */
        private const val VAD_POLL_MS = 150L
        /** Ambient-noise calibration window at the start of each listen. */
        private const val VAD_CALIBRATE_MS = 500L
        /** Speech-start floor; effective threshold = max(this, ambient×3). */
        private const val VAD_MIN_THRESHOLD = 1500
        private const val VAD_AMBIENT_FACTOR = 3
        /** Continuous sub-threshold time that ends the utterance. */
        private const val VAD_END_SILENCE_MS = 1800L
        /** No speech at all within this → gentle stop (never an error loop). */
        private const val VAD_NO_SPEECH_MS = 8000L
        /** Hard cap: stop and send whatever was captured. */
        private const val VAD_MAX_UTTERANCE_MS = 45_000L
        /** Less voiced time than this = breath/rustle → discard silently. */
        private const val VAD_MIN_VOICED_MS = 400L
    }

    // ------------------------------------------------------------- chat model

    private enum class Role { OWNER, AGENT, SYSTEM }

    /** [pending] = live speech partial: muted bubble, never persisted. */
    private class Msg(
        var role: Role,
        var text: String,
        var failed: Boolean = false,
        var pending: Boolean = false
    )

    /**
     * Hands-free loop states. SPEAKING (agent TTS playing) → on completion →
     * LISTENING (MediaRecorder + amplitude VAD, muted "🎙 …" bubble once
     * speech starts) → end-of-speech → THINKING (one voiceMessage call does
     * Whisper transcript + reply + TTS; typing dots) → reply → SPEAKING.
     * Silence, timeouts, or any user action drop to IDLE (strip hidden); the
     * loop then waits for the next agent reply or an explicit tap.
     */
    private enum class VoiceState { IDLE, SPEAKING, LISTENING, THINKING }

    private val msgs = mutableListOf<Msg>()
    private var typing = false

    private lateinit var recycler: RecyclerView
    private lateinit var adapter: ChatAdapter
    private lateinit var input: TextInputEditText
    private lateinit var sendButton: MaterialButton
    private lateinit var micButton: MaterialButton
    private lateinit var cameraButton: MaterialButton
    private lateinit var replayButton: MaterialButton
    private lateinit var voiceToggle: MaterialButton
    private lateinit var recordBar: View
    private lateinit var recordDot: TextView
    private lateinit var recordTimer: TextView
    private lateinit var recordHint: TextView
    private lateinit var recordCancel: MaterialButton
    private lateinit var lockPill: View
    private lateinit var voiceStrip: View
    private lateinit var voiceDot: TextView
    private lateinit var voiceLabel: TextView

    private var recorder: MediaRecorder? = null
    private var player: MediaPlayer? = null
    private var recordStart = 0L
    // --- hold-to-record gesture state ---
    private var recordLocked = false
    private var micDownX = 0f
    private var micDownY = 0f
    private val timerHandler = Handler(Looper.getMainLooper())
    private val voiceHandler = Handler(Looper.getMainLooper())
    private var recordPulse: ObjectAnimator? = null
    private var statusPulse: ObjectAnimator? = null
    private var pendingDone = false

    // --- voice-mode state machine ---
    private var voiceState = VoiceState.IDLE
    private var voiceModeOn = false
    private var interviewDone = false
    private var pendingVoiceMsg: Msg? = null
    private val uiPrefs by lazy { getSharedPreferences(UI_PREFS, MODE_PRIVATE) }

    // --- hands-free capture + VAD (separate from the push-to-talk recorder) ---
    private var vadRecorder: MediaRecorder? = null
    private var vadStartAt = 0L
    private var vadFirstPollDone = false
    private var vadAmbientMax = 0
    private var vadThreshold = 0
    private var vadSpeechStarted = false
    private var vadSpeechStartAt = 0L
    private var vadLastVoicedAt = 0L

    private val voiceFile: File by lazy { File(cacheDir, "voice.m4a") }
    private val replyWav: File by lazy { File(cacheDir, "reply.wav") }
    private val photoFile: File by lazy { File(cacheDir, "catalog.jpg") }

    // The mic is a gesture surface (hold/slide/swipe); performClick fires on
    // the send paths, and locked mode gives single-tap send for a11y users.
    @android.annotation.SuppressLint("ClickableViewAccessibility")
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        // Chat only exists for a registered business: registration (and OTP)
        // live in RegistrationActivity, never here.
        if (!Prefs.serverConfigured(this)) {
            startActivity(Intent(this, RegistrationActivity::class.java))
            finish()
            return
        }

        setContentView(R.layout.activity_onboarding)
        // Where the business is: asked once, right after registration, so the
        // card can say "Miraflores" before the interview even gets there.
        if (!LocationHelper.asked(this) && !LocationHelper.granted(this)) LocationHelper.ask(this)
        else LocationHelper.sync(this, bearer = true)
        recycler = findViewById(R.id.chat_recycler)
        input = findViewById(R.id.chat_input)
        sendButton = findViewById(R.id.chat_send)
        micButton = findViewById(R.id.chat_mic)
        cameraButton = findViewById(R.id.chat_camera)
        replayButton = findViewById(R.id.chat_replay)
        voiceToggle = findViewById(R.id.chat_voice_toggle)
        recordBar = findViewById(R.id.record_bar)
        recordDot = findViewById(R.id.record_dot)
        recordTimer = findViewById(R.id.record_timer)
        recordHint = findViewById(R.id.record_hint)
        recordCancel = findViewById(R.id.record_cancel)
        lockPill = findViewById(R.id.lock_pill)
        voiceStrip = findViewById(R.id.voice_strip)
        voiceDot = findViewById(R.id.voice_dot)
        voiceLabel = findViewById(R.id.voice_label)

        adapter = ChatAdapter()
        recycler.layoutManager = LinearLayoutManager(this).apply { stackFromEnd = true }
        recycler.adapter = adapter

        sendButton.setOnClickListener { send() }
        // WhatsApp-style mic: hold to record, release to send, slide left to
        // cancel, swipe up to lock; when locked, the mic itself becomes send.
        micButton.setOnTouchListener { v, ev -> onMicTouch(v, ev) }
        recordCancel.setOnClickListener { cancelHeldRecording() }
        cameraButton.setOnClickListener { showPhotoSourceDialog() }
        replayButton.setOnClickListener { replayLast() }
        voiceToggle.setOnClickListener { setVoiceMode(!voiceModeOn) }
        // Barge-in lite: tap the strip while the agent talks → answer now.
        voiceStrip.setOnClickListener {
            if (voiceState == VoiceState.SPEAKING) startVoiceListening()
        }

        // WhatsApp composer: mic when the box is empty, send once there's text.
        input.addTextChangedListener(object : android.text.TextWatcher {
            override fun beforeTextChanged(s: CharSequence?, a: Int, b: Int, c: Int) = Unit
            override fun onTextChanged(s: CharSequence?, a: Int, b: Int, c: Int) = Unit
            override fun afterTextChanged(s: android.text.Editable?) = updateComposerButtons()
        })
        updateComposerButtons()

        // Voice mode: default ON during the interview; a persisted user choice
        // wins. Capture is plain MediaRecorder + the server's Whisper path, so
        // every device with a microphone qualifies.
        voiceModeOn = uiPrefs.getBoolean(KEY_VOICE_MODE, true)
        updateVoiceToggle()

        // Restore the transcript so an interrupted owner never sees a blank chat.
        Prefs.chatTranscript(this)?.let { stored ->
            OnboardingTranscript.decode(stored, getString(R.string.typing_indicator)).forEach { (r, t) ->
                msgs.add(Msg(when (r) { OnboardingTranscript.OWNER -> Role.OWNER; OnboardingTranscript.AGENT -> Role.AGENT; else -> Role.SYSTEM }, t))
            }
            adapter.notifyDataSetChanged()
            scrollToBottom(smooth = false)
        }
        if (msgs.isEmpty()) {
            // Safety net: never open onto a silent agent — kick the interview
            // ourselves, invisibly (no owner bubble for the "hola").
            kickInterview()
        } else if (voiceModeOn && msgs.lastOrNull()?.role == Role.AGENT && !replyWav.exists()) {
            // Seeded greeting with no TTS: give the owner a beat to read it,
            // then open the mic — the loop must begin without a tap.
            voiceHandler.postDelayed({
                if (voiceState == VoiceState.IDLE && !typing) startVoiceListening()
            }, LISTEN_ON_OPEN_MS)
        }
    }

    // -------------------------------------------------- transcript persistence

    /**
     * Best-effort parse of one stored block back into a bubble role. The old
     * one-TextView build persisted blocks joined by \n\n with "🧑 "/"🟢 "
     * markers and bare status lines — the same format we keep writing, so old
     * transcripts load unchanged. Unknown lines become system lines; stale
     * typing-indicator blocks are dropped.
     */
    private fun parseLine(line: String): Msg? {
        val t = line.trim()
        if (t.isEmpty()) return null
        if (t == getString(R.string.typing_indicator)) return null
        return when {
            t.startsWith(OWNER_PREFIX) -> Msg(Role.OWNER, t.removePrefix(OWNER_PREFIX))
            t.startsWith(AGENT_PREFIX) -> Msg(Role.AGENT, t.removePrefix(AGENT_PREFIX))
            else -> Msg(Role.SYSTEM, t)
        }
    }

    private fun persist() {
        // Pending speech partials are ephemeral — never written to disk.
        val kept = msgs.filter { !it.pending }.map {
            when (it.role) {
                Role.OWNER -> OnboardingTranscript.OWNER
                Role.AGENT -> OnboardingTranscript.AGENT
                Role.SYSTEM -> OnboardingTranscript.SYSTEM
            } to it.text
        }
        Prefs.setChatTranscript(this, OnboardingTranscript.encode(kept))
    }

    // ------------------------------------------------------------ chat surface

    private fun addMsg(role: Role, text: String): Msg {
        val m = Msg(role, text)
        msgs.add(m)
        adapter.notifyItemInserted(msgs.size - 1)
        persist()
        scrollToBottom()
        return m
    }

    /** Add a line that may carry legacy 🧑/🟢 markers (e.g. resume hint). */
    private fun addParsed(line: String) {
        parseLine(line)?.let {
            msgs.add(it)
            adapter.notifyItemInserted(msgs.size - 1)
            persist()
            scrollToBottom()
        }
    }

    private fun changeMsg(m: Msg, block: (Msg) -> Unit) {
        block(m)
        val i = msgs.indexOf(m)
        if (i >= 0) adapter.notifyItemChanged(i)
        persist()
    }

    private fun showTyping() {
        if (typing) return
        typing = true
        adapter.notifyItemInserted(msgs.size)
        scrollToBottom()
        if (voiceModeOn && !interviewDone) setVoiceStatus(VoiceState.THINKING)
    }

    private fun hideTyping() {
        if (!typing) return
        typing = false
        adapter.notifyItemRemoved(msgs.size)
    }

    private fun scrollToBottom(smooth: Boolean = true) {
        val last = adapter.itemCount - 1
        if (last < 0) return
        recycler.post {
            if (smooth) recycler.smoothScrollToPosition(last)
            else recycler.scrollToPosition(last)
        }
    }

    private fun setBusy(busy: Boolean) {
        sendButton.isEnabled = !busy
        input.isEnabled = !busy
        micButton.isEnabled = !busy || recorder != null
        cameraButton.isEnabled = !busy
    }

    // ------------------------------------------------------------------- text

    private fun send() {
        val text = input.text.toString().trim()
        if (text.isEmpty()) return
        interruptVoiceLoop() // typing wins over any TTS/recognizer in progress
        val mine = addMsg(Role.OWNER, text)
        showTyping()
        setBusy(true)
        ServerClient.EXECUTOR.execute {
            val resp = ServerClient.onboardingMessage(this, text)
            runOnUiThread {
                // Only clear the box once the message actually made it through.
                if (resp != null) input.setText("")
                handleAgentResponse(resp, retryTarget = mine)
            }
        }
    }

    /** "Reintentar" on a failed bubble — resends that exact text. */
    private fun resend(m: Msg) {
        if (!sendButton.isEnabled) return // a send is already in flight
        interruptVoiceLoop()
        changeMsg(m) { it.failed = false }
        showTyping()
        setBusy(true)
        ServerClient.EXECUTOR.execute {
            val resp = ServerClient.onboardingMessage(this, m.text)
            runOnUiThread { handleAgentResponse(resp, retryTarget = m) }
        }
    }

    // ------------------------------------------------------------- voice mode
    //
    // WhatsApp-style push-to-talk. The mic button is a gesture surface:
    //   hold → record        release → send        quick tap → "hold" hint
    //   slide left → cancel  swipe up → lock (hands-free; mic becomes ➤)
    // While locked, tapping the mic sends and the bar grows a Cancel button.

    /** WhatsApp composer rule: mic when the box is empty, send when it isn't. */
    private fun updateComposerButtons() {
        val recording = recorder != null
        val hasText = !input.text.isNullOrBlank()
        micButton.visibility = if (recording || !hasText) View.VISIBLE else View.GONE
        sendButton.visibility = if (!recording && hasText) View.VISIBLE else View.GONE
    }

    private fun onMicTouch(v: View, ev: android.view.MotionEvent): Boolean {
        val density = resources.displayMetrics.density
        when (ev.actionMasked) {
            android.view.MotionEvent.ACTION_DOWN -> {
                if (recordLocked && recorder != null) {
                    // Locked mode: the mic IS the send button now.
                    v.performClick()
                    stopRecordingAndSend()
                    return true
                }
                if (!v.isEnabled || recorder != null) return true
                if (ContextCompat.checkSelfPermission(this, Manifest.permission.RECORD_AUDIO)
                    != PackageManager.PERMISSION_GRANTED
                ) {
                    ActivityCompat.requestPermissions(
                        this, arrayOf(Manifest.permission.RECORD_AUDIO), 71
                    )
                    return true
                }
                interruptVoiceLoop() // push-to-talk takes the mic from the loop
                v.parent.requestDisallowInterceptTouchEvent(true)
                micDownX = ev.rawX
                micDownY = ev.rawY
                startHeldRecording()
            }
            android.view.MotionEvent.ACTION_MOVE -> {
                if (recorder == null || recordLocked) return true
                val dx = ev.rawX - micDownX
                val dy = ev.rawY - micDownY
                val cancelPx = CANCEL_DRAG_DP * density
                // The hint chases the finger and fades out — the same tell
                // WhatsApp gives that letting go here will throw it away.
                val pull = dx.coerceIn(-cancelPx, 0f)
                recordHint.translationX = pull * 0.5f
                recordHint.alpha = 1f + (pull / cancelPx) * 0.8f
                if (dx <= -cancelPx) {
                    cancelHeldRecording()
                    return true
                }
                if (dy <= -(LOCK_DRAG_DP * density)) {
                    lockHeldRecording()
                    return true
                }
            }
            android.view.MotionEvent.ACTION_UP,
            android.view.MotionEvent.ACTION_CANCEL -> {
                if (recorder == null || recordLocked) return true
                if (System.currentTimeMillis() - recordStart < MIN_HOLD_MS) {
                    // A tap, not a hold — teach the gesture instead of sending
                    // a half-syllable to Whisper.
                    cancelHeldRecording()
                    Toast.makeText(
                        this, getString(R.string.chat_hold_to_record), Toast.LENGTH_SHORT
                    ).show()
                } else {
                    v.performClick()
                    stopRecordingAndSend()
                }
            }
        }
        return true
    }

    private fun startHeldRecording() {
        try {
            player?.release(); player = null
            @Suppress("DEPRECATION")
            recorder = MediaRecorder().apply {
                setAudioSource(MediaRecorder.AudioSource.MIC)
                setOutputFormat(MediaRecorder.OutputFormat.MPEG_4)
                setAudioEncoder(MediaRecorder.AudioEncoder.AAC)
                setAudioSamplingRate(44100)
                setAudioEncodingBitRate(96000)
                setOutputFile(voiceFile.absolutePath)
                prepare()
                start()
            }
        } catch (e: Exception) {
            recorder = null
            resetRecordUi()
            Toast.makeText(this, getString(R.string.mic_error), Toast.LENGTH_LONG).show()
            return
        }
        recordStart = System.currentTimeMillis()
        recordLocked = false
        micButton.performHapticFeedback(android.view.HapticFeedbackConstants.LONG_PRESS)

        // Held UI: bar with pulsing dot + timer + slide-to-cancel hint, the
        // mic swells under the finger, and the lock pill floats above it.
        recordBar.visibility = View.VISIBLE
        recordHint.text = getString(R.string.chat_slide_cancel_hint)
        recordHint.translationX = 0f
        recordHint.alpha = 1f
        recordCancel.visibility = View.GONE
        recordTimer.text = getString(R.string.chat_timer_zero)
        micButton.animate().scaleX(1.45f).scaleY(1.45f).setDuration(120).start()
        micButton.backgroundTintList =
            ContextCompat.getColorStateList(this, R.color.agente_error_container)
        micButton.setTextColor(ContextCompat.getColor(this, R.color.agente_error))
        micButton.contentDescription = getString(R.string.chat_a11y_stop_recording)
        lockPill.visibility = View.VISIBLE
        recordPulse = ObjectAnimator.ofFloat(recordDot, View.ALPHA, 1f, 0.2f).apply {
            duration = 550
            repeatCount = ObjectAnimator.INFINITE
            repeatMode = ObjectAnimator.REVERSE
            start()
        }
        updateComposerButtons()
        tickTimer()
    }

    /** Swipe-up latch: the finger can leave; the mic becomes the send button. */
    private fun lockHeldRecording() {
        if (recorder == null || recordLocked) return
        recordLocked = true
        micButton.performHapticFeedback(android.view.HapticFeedbackConstants.KEYBOARD_TAP)
        micButton.animate().scaleX(1f).scaleY(1f).setDuration(120).start()
        micButton.text = getString(R.string.chat_send_glyph)
        micButton.backgroundTintList =
            ContextCompat.getColorStateList(this, R.color.agente_primary)
        micButton.setTextColor(ContextCompat.getColor(this, R.color.agente_on_primary))
        micButton.contentDescription = getString(R.string.chat_a11y_send_voice)
        recordHint.translationX = 0f
        recordHint.alpha = 1f
        recordHint.text = getString(R.string.chat_locked_hint)
        recordCancel.visibility = View.VISIBLE
        lockPill.visibility = View.GONE
    }

    /** Slide-left / too-short / Cancel button: discard the capture entirely. */
    private fun cancelHeldRecording() {
        timerHandler.removeCallbacksAndMessages(null)
        try { recorder?.stop() } catch (_: Exception) {}
        recorder?.release()
        recorder = null
        voiceFile.delete()
        resetRecordUi()
    }

    /** Composer back to its resting state, whatever just happened. */
    private fun resetRecordUi() {
        recordPulse?.cancel(); recordPulse = null
        recordDot.alpha = 1f
        recordBar.visibility = View.GONE
        recordCancel.visibility = View.GONE
        lockPill.visibility = View.GONE
        recordLocked = false
        recordHint.translationX = 0f
        recordHint.alpha = 1f
        micButton.animate().cancel()
        micButton.scaleX = 1f
        micButton.scaleY = 1f
        micButton.text = getString(R.string.chat_mic_glyph)
        micButton.contentDescription = getString(R.string.chat_a11y_mic)
        micButton.backgroundTintList =
            ContextCompat.getColorStateList(this, R.color.agente_primary_container)
        micButton.setTextColor(ContextCompat.getColor(this, R.color.agente_on_primary_container))
        updateComposerButtons()
    }

    private fun tickTimer() {
        if (recorder == null) return
        val secs = (System.currentTimeMillis() - recordStart) / 1000
        recordTimer.text = String.format("%d:%02d", secs / 60, secs % 60)
        timerHandler.postDelayed({ tickTimer() }, 500)
    }

    private fun onLocationPermission(requestCode: Int, grantResults: IntArray): Boolean {
        if (requestCode != LocationHelper.REQUEST_CODE) return false
        if (grantResults.firstOrNull() == android.content.pm.PackageManager.PERMISSION_GRANTED) {
            LocationHelper.sync(this, bearer = true, force = true)
        }
        return true
    }

    override fun onRequestPermissionsResult(
        requestCode: Int, permissions: Array<out String>, grantResults: IntArray
    ) {
        super.onRequestPermissionsResult(requestCode, permissions, grantResults)
        if (onLocationPermission(requestCode, grantResults)) return
        if (requestCode == RC_VOICE_LISTEN) {
            // First hands-free listen. Grant → open the mic; denial → text
            // mode, no nagging dialog (session-only, so a later grant can
            // bring voice back without digging through settings).
            if (grantResults.firstOrNull() == PackageManager.PERMISSION_GRANTED) {
                startVoiceListening()
            } else {
                setVoiceMode(false, persistChoice = false)
                addSystemLineOnce(getString(R.string.voice_perm_denied))
            }
            return
        }
        if (requestCode != 71) return
        if (grantResults.firstOrNull() == PackageManager.PERMISSION_GRANTED) {
            // The finger that triggered the request is long gone — teach the
            // gesture rather than surprise-recording.
            Toast.makeText(this, getString(R.string.chat_hold_to_record), Toast.LENGTH_SHORT)
                .show()
        } else {
            MaterialAlertDialogBuilder(this)
                .setTitle(R.string.mic_perm_title)
                .setMessage(R.string.mic_perm_msg)
                .setPositiveButton(R.string.permission_open_settings) { _, _ ->
                    startActivity(
                        Intent(Settings.ACTION_APPLICATION_DETAILS_SETTINGS,
                               Uri.parse("package:$packageName"))
                    )
                }
                .setNegativeButton(android.R.string.cancel, null)
                .show()
        }
    }

    private fun stopRecordingAndSend() {
        timerHandler.removeCallbacksAndMessages(null)
        try { recorder?.stop() } catch (_: Exception) {}
        recorder?.release()
        recorder = null
        resetRecordUi()
        if (!voiceFile.exists() || voiceFile.length() < 1000) {
            Toast.makeText(this, getString(R.string.voice_too_short), Toast.LENGTH_SHORT).show()
            return
        }
        val voiceMsg = addMsg(Role.OWNER, "🎤 …")
        showTyping()
        setBusy(true)
        val bytes = voiceFile.readBytes()
        ServerClient.EXECUTOR.execute {
            val resp = ServerClient.voiceMessage(this, bytes)
            runOnUiThread {
                resp?.optString("transcript")?.takeIf { it.isNotEmpty() }?.let { t ->
                    // Swap the placeholder for what the owner actually said.
                    changeMsg(voiceMsg) { it.text = "🎤 $t" }
                }
                handleAgentResponse(resp, retryTarget = null)
            }
        }
    }

    /**
     * Play the last reply wav. In voice mode this is the SPEAKING leg of the
     * hands-free loop: playback completion opens the mic automatically.
     * @return true if playback actually started.
     */
    private fun replayLast(): Boolean {
        if (!replyWav.exists()) return false
        if (voiceState == VoiceState.LISTENING) cancelVoiceListening()
        return try {
            player?.release()
            player = MediaPlayer().apply {
                setDataSource(replyWav.absolutePath)
                setOnCompletionListener {
                    if (voiceState == VoiceState.SPEAKING) {
                        if (voiceModeOn && !interviewDone) startVoiceListening()
                        else setVoiceStatus(VoiceState.IDLE)
                    }
                }
                prepare(); start()
            }
            if (voiceModeOn) setVoiceStatus(VoiceState.SPEAKING)
            true
        } catch (_: Exception) {
            setVoiceStatus(VoiceState.IDLE)
            Toast.makeText(this, getString(R.string.audio_error), Toast.LENGTH_SHORT).show()
            false
        }
    }

    /** @return true if agent TTS audio started playing (loop continues there). */
    private fun playIfPresent(resp: org.json.JSONObject): Boolean {
        val b64 = resp.optString("audioBase64").takeIf { it.isNotEmpty() && it != "null" }
            ?: return false
        return try {
            replyWav.writeBytes(Base64.decode(b64, Base64.DEFAULT))
            replayButton.visibility = View.VISIBLE
            replayLast()
        } catch (_: Exception) {
            Toast.makeText(this, getString(R.string.audio_error), Toast.LENGTH_SHORT).show()
            false
        }
    }

    // -------------------------------------------------------- catalog photo

    /**
     * Owner sends a photo of their catalog/menu; the server extracts items and
     * prices. Camera goes through ACTION_IMAGE_CAPTURE + FileProvider (full
     * size, no CAMERA permission), gallery through ACTION_GET_CONTENT.
     */
    private fun showPhotoSourceDialog() {
        MaterialAlertDialogBuilder(this)
            .setTitle(R.string.catalog_photo_title)
            .setItems(
                arrayOf(
                    getString(R.string.catalog_photo_take),
                    getString(R.string.catalog_photo_gallery)
                )
            ) { _, which ->
                if (which == 0) launchCamera() else launchGallery()
            }
            .setNegativeButton(android.R.string.cancel, null)
            .show()
    }

    private fun launchCamera() {
        photoFile.delete()
        val uri = FileProvider.getUriForFile(this, "$packageName.fileprovider", photoFile)
        val intent = Intent(MediaStore.ACTION_IMAGE_CAPTURE)
            .putExtra(MediaStore.EXTRA_OUTPUT, uri)
            .addFlags(
                Intent.FLAG_GRANT_READ_URI_PERMISSION or
                Intent.FLAG_GRANT_WRITE_URI_PERMISSION
            )
        // Grant flags only cover data/clipData, not extras — mirror the uri
        // there so every camera app can actually write to it.
        intent.clipData = ClipData.newRawUri("output", uri)
        try {
            startActivityForResult(intent, RC_CATALOG_CAMERA)
        } catch (_: Exception) {
            Toast.makeText(this, getString(R.string.catalog_photo_no_app), Toast.LENGTH_LONG).show()
        }
    }

    private fun launchGallery() {
        val intent = Intent(Intent.ACTION_GET_CONTENT)
            .setType("image/*")
            .addCategory(Intent.CATEGORY_OPENABLE)
        try {
            startActivityForResult(intent, RC_CATALOG_GALLERY)
        } catch (_: Exception) {
            Toast.makeText(this, getString(R.string.catalog_photo_no_app), Toast.LENGTH_LONG).show()
        }
    }

    @Deprecated("Deprecated in Java")
    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        @Suppress("DEPRECATION")
        super.onActivityResult(requestCode, resultCode, data)
        if (resultCode != RESULT_OK) return
        val raw: ByteArray? = when (requestCode) {
            RC_CATALOG_CAMERA ->
                photoFile.takeIf { it.exists() && it.length() > 0 }?.readBytes()
            RC_CATALOG_GALLERY ->
                data?.data?.let { uri ->
                    runCatching {
                        contentResolver.openInputStream(uri)?.use { it.readBytes() }
                    }.getOrNull()
                }
            else -> return
        }
        if (raw == null || raw.isEmpty()) {
            Toast.makeText(this, getString(R.string.catalog_photo_unreadable), Toast.LENGTH_LONG).show()
            return
        }
        sendCatalogPhoto(raw)
    }

    private fun sendCatalogPhoto(raw: ByteArray) {
        val status = addMsg(Role.SYSTEM, "📷 ⏳ ${getString(R.string.catalog_photo_reading)}")
        setBusy(true)
        ServerClient.EXECUTOR.execute {
            val jpeg = prepareCatalogJpeg(raw)
            val resp = jpeg?.let { ServerClient.catalogPhoto(this, it) }
            runOnUiThread {
                setBusy(false)
                photoFile.delete()
                if (resp == null) { // undecodable image, never left the phone
                    changeMsg(status) {
                        it.text = "⚠️ ${getString(R.string.catalog_photo_unreadable)}"
                    }
                    return@runOnUiThread
                }
                handleCatalogResponse(resp, status)
            }
        }
    }

    /**
     * Downscale so the longest edge is ≤ [MAX_PHOTO_EDGE] px and re-encode as
     * JPEG q85 — menu photos are all the server needs, not 12MP originals.
     * inSampleSize does the cheap power-of-two step, an exact scale finishes,
     * and the EXIF orientation is baked in so the server never sees a
     * sideways menu. Null = not a decodable image.
     */
    private fun prepareCatalogJpeg(raw: ByteArray): ByteArray? {
        return try {
            val bounds = BitmapFactory.Options().apply { inJustDecodeBounds = true }
            BitmapFactory.decodeByteArray(raw, 0, raw.size, bounds)
            if (bounds.outWidth <= 0 || bounds.outHeight <= 0) return null
            var sample = 1
            while (maxOf(bounds.outWidth, bounds.outHeight) / (sample * 2) >= MAX_PHOTO_EDGE) {
                sample *= 2
            }
            val opts = BitmapFactory.Options().apply { inSampleSize = sample }
            var bmp = BitmapFactory.decodeByteArray(raw, 0, raw.size, opts) ?: return null
            val longest = maxOf(bmp.width, bmp.height)
            if (longest > MAX_PHOTO_EDGE) {
                val scale = MAX_PHOTO_EDGE.toFloat() / longest
                bmp = Bitmap.createScaledBitmap(
                    bmp,
                    (bmp.width * scale).toInt().coerceAtLeast(1),
                    (bmp.height * scale).toInt().coerceAtLeast(1),
                    true
                )
            }
            val rotation = try {
                when (ExifInterface(ByteArrayInputStream(raw)).getAttributeInt(
                    ExifInterface.TAG_ORIENTATION, ExifInterface.ORIENTATION_NORMAL
                )) {
                    ExifInterface.ORIENTATION_ROTATE_90 -> 90f
                    ExifInterface.ORIENTATION_ROTATE_180 -> 180f
                    ExifInterface.ORIENTATION_ROTATE_270 -> 270f
                    else -> 0f
                }
            } catch (_: Exception) { 0f }
            if (rotation != 0f) {
                val m = Matrix().apply { postRotate(rotation) }
                bmp = Bitmap.createBitmap(bmp, 0, 0, bmp.width, bmp.height, m, true)
            }
            val out = ByteArrayOutputStream()
            bmp.compress(Bitmap.CompressFormat.JPEG, 85, out)
            out.toByteArray()
        } catch (e: Exception) {
            android.util.Log.w("Onboarding", "catalog photo decode failed", e)
            null
        }
    }

    private fun handleCatalogResponse(resp: ServerClient.Response, status: Msg) {
        val text = when {
            resp.code in 200..299 && resp.json != null -> {
                val items = resp.json.optJSONArray("items")
                val total = items?.length() ?: 0
                val preview = StringBuilder()
                for (i in 0 until minOf(total, 3)) {
                    val item = items?.optJSONObject(i) ?: continue
                    if (preview.isNotEmpty()) preview.append(", ")
                    preview.append(item.optString("name"))
                        .append(" ").append(Prefs.money(this, item.optDouble("price", 0.0)))
                }
                if (total > 3) preview.append(", …")
                val note = resp.json.optString("note").takeIf { it.isNotEmpty() }
                    ?: getString(R.string.catalog_photo_saved, resp.json.optInt("count", total))
                "✅ $note" + if (preview.isNotEmpty()) " — $preview" else ""
            }
            resp.code == 422 -> "⚠️ ${getString(R.string.catalog_photo_unreadable)}"
            resp.code == 503 -> "⚠️ ${getString(R.string.catalog_photo_unavailable)}"
            resp.code == 429 -> "⚠️ ${getString(R.string.verify_throttled)}"
            else -> "⚠️ ${getString(R.string.server_error)}"
        }
        changeMsg(status) { it.text = text }
        scrollToBottom()
    }

    // ----------------------------------------------------------------- shared

    /**
     * [retryTarget] is the owner bubble a null response should mark as failed
     * (tap → resend). Voice sends pass null: their bytes are gone once the
     * recorder file is rewritten, so failure becomes a readable system line.
     */
    private fun handleAgentResponse(resp: org.json.JSONObject?, retryTarget: Msg?) {
        setBusy(false)
        hideTyping()
        if (resp == null) {
            setVoiceStatus(VoiceState.IDLE) // loop stops; the user acts next
            if (retryTarget != null) {
                changeMsg(retryTarget) { it.failed = true }
            } else {
                addMsg(Role.SYSTEM, "⚠️ ${getString(R.string.server_error)}")
            }
            return
        }
        addMsg(Role.AGENT, resp.optString("agentResponse"))
        val finishing = resp.optString("action") == "finish_onboarding"
        if (finishing) interviewDone = true // audio still plays; loop stops
        val audioStarted = playIfPresent(resp)
        if (finishing) {
            setVoiceStatus(VoiceState.IDLE)
            addMsg(Role.SYSTEM, "✅ ${getString(R.string.onboarding_saved)}")
            showDoneSheet()
        } else if (!audioStarted) {
            setVoiceStatus(VoiceState.IDLE)
            if (voiceModeOn) {
                // Reply without TTS: a beat to read it, then open the mic —
                // the loop must not die just because audio was missing.
                voiceHandler.postDelayed({
                    if (voiceState == VoiceState.IDLE && recorder == null && !typing) {
                        startVoiceListening()
                    }
                }, LISTEN_AFTER_TEXT_MS)
            }
        }
    }

    // ------------------------------------------------- hands-free voice loop

    /** Flip voice mode. [persistChoice] false = session-only (perm denials). */
    private fun setVoiceMode(on: Boolean, persistChoice: Boolean = true) {
        voiceModeOn = on
        if (persistChoice) uiPrefs.edit().putBoolean(KEY_VOICE_MODE, voiceModeOn).apply()
        if (!voiceModeOn) interruptVoiceLoop() // silence everything, instantly
        updateVoiceToggle()
    }

    private fun updateVoiceToggle() {
        voiceToggle.text = getString(
            if (voiceModeOn) R.string.chat_voice_on_glyph else R.string.chat_voice_off_glyph
        )
        voiceToggle.contentDescription = getString(
            if (voiceModeOn) R.string.chat_a11y_voice_mute else R.string.chat_a11y_voice_unmute
        )
    }

    /** Stop TTS playback + recognizer, drop any partial bubble → IDLE. */
    private fun interruptVoiceLoop() {
        voiceHandler.removeCallbacksAndMessages(null)
        player?.release(); player = null
        cancelVoiceListening()
        if (voiceState != VoiceState.IDLE) setVoiceStatus(VoiceState.IDLE)
    }

    private fun cancelVoiceListening() {
        stopVadRecorder(discardCapture = true)
        discardPendingVoice()
        if (voiceState == VoiceState.LISTENING) setVoiceStatus(VoiceState.IDLE)
    }

    /**
     * LISTENING leg: open the mic with the exact push-to-talk MediaRecorder
     * config and run a DIY voice-activity detector over getMaxAmplitude().
     * No on-device model needed — end-of-speech ships the m4a through the
     * server's Whisper path (transcript + reply + TTS in one call).
     */
    private fun startVoiceListening() {
        if (!voiceModeOn || interviewDone) return
        if (recorder != null || vadRecorder != null || voiceState == VoiceState.LISTENING) return
        if (ContextCompat.checkSelfPermission(this, Manifest.permission.RECORD_AUDIO)
            != PackageManager.PERMISSION_GRANTED
        ) {
            ActivityCompat.requestPermissions(
                this, arrayOf(Manifest.permission.RECORD_AUDIO), RC_VOICE_LISTEN
            )
            return
        }
        player?.release(); player = null
        try {
            @Suppress("DEPRECATION")
            vadRecorder = MediaRecorder().apply {
                setAudioSource(MediaRecorder.AudioSource.MIC)
                setOutputFormat(MediaRecorder.OutputFormat.MPEG_4)
                setAudioEncoder(MediaRecorder.AudioEncoder.AAC)
                setAudioSamplingRate(44100)
                setAudioEncodingBitRate(96000)
                setOutputFile(voiceFile.absolutePath)
                prepare()
                start()
            }
        } catch (_: Exception) {
            vadRecorder?.release(); vadRecorder = null
            setVoiceStatus(VoiceState.IDLE)
            addSystemLineOnce(getString(R.string.voice_not_heard))
            return
        }
        vadStartAt = SystemClock.elapsedRealtime()
        vadFirstPollDone = false
        vadAmbientMax = 0
        vadThreshold = 0
        vadSpeechStarted = false
        vadSpeechStartAt = 0L
        vadLastVoicedAt = 0L
        setVoiceStatus(VoiceState.LISTENING)
        voiceHandler.postDelayed({ pollVad() }, VAD_POLL_MS)
    }

    /**
     * One VAD tick every [VAD_POLL_MS]. getMaxAmplitude() reports the max
     * since the previous call: the first read is always 0 (discarded), the
     * calibration window measures ambient noise, then speech starts at
     * max([VAD_MIN_THRESHOLD], ambient×[VAD_AMBIENT_FACTOR]) and ends after
     * [VAD_END_SILENCE_MS] of continuous quiet.
     */
    private fun pollVad() {
        val rec = vadRecorder ?: return
        val now = SystemClock.elapsedRealtime()
        val amp = try { rec.maxAmplitude } catch (_: Exception) { 0 }
        when {
            !vadFirstPollDone -> vadFirstPollDone = true
            now - vadStartAt <= VAD_CALIBRATE_MS ->
                vadAmbientMax = maxOf(vadAmbientMax, amp)
            else -> {
                if (vadThreshold == 0) {
                    vadThreshold = maxOf(VAD_MIN_THRESHOLD, vadAmbientMax * VAD_AMBIENT_FACTOR)
                }
                if (amp >= vadThreshold) {
                    if (!vadSpeechStarted) {
                        vadSpeechStarted = true
                        vadSpeechStartAt = now
                        showVoicePendingBubble()
                    }
                    vadLastVoicedAt = now
                }
                if (!vadSpeechStarted && now - vadStartAt >= VAD_NO_SPEECH_MS) {
                    abortListening(tellOwner = true) // nobody spoke: gentle stop
                    return
                }
                if (vadSpeechStarted) {
                    if (now - vadStartAt >= VAD_MAX_UTTERANCE_MS) {
                        finishListening() // hard cap — send what we have
                        return
                    }
                    if (now - vadLastVoicedAt >= VAD_END_SILENCE_MS) {
                        if (vadLastVoicedAt - vadSpeechStartAt < VAD_MIN_VOICED_MS) {
                            abortListening(tellOwner = false) // breath/rustle
                        } else {
                            finishListening()
                        }
                        return
                    }
                }
            }
        }
        voiceHandler.postDelayed({ pollVad() }, VAD_POLL_MS)
    }

    private fun stopVadRecorder(discardCapture: Boolean) {
        val rec = vadRecorder ?: return
        vadRecorder = null // poller sees null and dies on its next tick
        try { rec.stop() } catch (_: Exception) {}
        rec.release()
        if (discardCapture) voiceFile.delete()
    }

    /** Drop the capture; [tellOwner] leaves the gentle "didn't hear you" line. */
    private fun abortListening(tellOwner: Boolean) {
        stopVadRecorder(discardCapture = true)
        discardPendingVoice()
        setVoiceStatus(VoiceState.IDLE)
        if (tellOwner) addSystemLineOnce(getString(R.string.voice_not_heard))
    }

    /**
     * End-of-speech: close the file and ship it down the existing Whisper
     * path — exactly the push-to-talk flow, so the "🎤 …" bubble gets the
     * transcript swapped in when the server answers.
     */
    private fun finishListening() {
        stopVadRecorder(discardCapture = false)
        if (!voiceFile.exists() || voiceFile.length() < 1000) {
            discardPendingVoice()
            setVoiceStatus(VoiceState.IDLE)
            return
        }
        val voiceMsg = pendingVoiceMsg?.also { m ->
            pendingVoiceMsg = null
            changeMsg(m) { it.pending = false; it.text = "🎤 …" }
        } ?: addMsg(Role.OWNER, "🎤 …")
        showTyping()
        setBusy(true)
        val bytes = voiceFile.readBytes()
        ServerClient.EXECUTOR.execute {
            val resp = ServerClient.voiceMessage(this, bytes)
            runOnUiThread {
                resp?.optString("transcript")?.takeIf { it.isNotEmpty() }?.let { t ->
                    changeMsg(voiceMsg) { it.text = "🎤 $t" }
                }
                handleAgentResponse(resp, retryTarget = null)
            }
        }
    }

    /** Muted "🎙 …" bubble while capturing — no live partials on Whisper. */
    private fun showVoicePendingBubble() {
        if (pendingVoiceMsg != null) return
        val nm = Msg(Role.OWNER, "🎙 …", pending = true)
        pendingVoiceMsg = nm
        msgs.add(nm)
        adapter.notifyItemInserted(msgs.size - 1)
        scrollToBottom()
    }

    private fun discardPendingVoice() {
        val m = pendingVoiceMsg ?: return
        pendingVoiceMsg = null
        val i = msgs.indexOf(m)
        if (i >= 0) {
            msgs.removeAt(i)
            adapter.notifyItemRemoved(i)
        }
    }

    /** One quiet status line, never repeated back-to-back. */
    private fun addSystemLineOnce(text: String) {
        if (msgs.lastOrNull()?.text == text) return
        addMsg(Role.SYSTEM, text)
    }

    // ------------------------------------------------------ voice status strip

    /**
     * Slim strip above the composer — the whole voice UI. One dot + one label,
     * recolored and pulsed per state; hidden when idle or muted. During
     * SPEAKING the strip itself is the barge-in tap target.
     */
    private fun setVoiceStatus(state: VoiceState) {
        voiceState = state
        statusPulse?.cancel(); statusPulse = null
        voiceDot.alpha = 1f
        voiceLabel.alpha = 1f
        val visible = state != VoiceState.IDLE && voiceModeOn && !interviewDone
        voiceStrip.visibility = if (visible) View.VISIBLE else View.GONE
        if (!visible) return

        fun color(res: Int) = ContextCompat.getColor(this, res)
        when (state) {
            VoiceState.SPEAKING -> {
                voiceLabel.text = getString(R.string.voice_status_speaking)
                voiceLabel.setTextColor(color(R.color.agente_primary))
                voiceDot.setTextColor(color(R.color.agente_primary))
                voiceStrip.isClickable = true
                voiceStrip.contentDescription = getString(R.string.chat_a11y_voice_interrupt)
                statusPulse = ObjectAnimator.ofFloat(voiceLabel, View.ALPHA, 1f, 0.55f).apply {
                    duration = 700
                    repeatCount = ObjectAnimator.INFINITE
                    repeatMode = ObjectAnimator.REVERSE
                    start()
                }
            }
            VoiceState.LISTENING -> {
                voiceLabel.text = getString(R.string.voice_status_listening)
                voiceLabel.setTextColor(color(R.color.agente_on_surface))
                voiceDot.setTextColor(color(R.color.agente_error))
                voiceStrip.isClickable = false
                voiceStrip.contentDescription = getString(R.string.voice_status_listening)
                statusPulse = ObjectAnimator.ofFloat(voiceDot, View.ALPHA, 1f, 0.2f).apply {
                    duration = 550
                    repeatCount = ObjectAnimator.INFINITE
                    repeatMode = ObjectAnimator.REVERSE
                    start()
                }
            }
            VoiceState.THINKING -> {
                voiceLabel.text = getString(R.string.voice_status_thinking)
                voiceLabel.setTextColor(color(R.color.agente_on_surface_muted))
                voiceDot.setTextColor(color(R.color.agente_on_surface_muted))
                voiceStrip.isClickable = false
                voiceStrip.contentDescription = getString(R.string.voice_status_thinking)
                statusPulse = ObjectAnimator.ofFloat(voiceDot, View.ALPHA, 0.3f, 1f).apply {
                    duration = 450
                    repeatCount = ObjectAnimator.INFINITE
                    repeatMode = ObjectAnimator.REVERSE
                    start()
                }
            }
            VoiceState.IDLE -> Unit
        }
    }

    // --------------------------------------------------- interview safety net

    /**
     * Blank transcript on a configured device: the agent must speak first.
     * Send a silent "hola" through the normal onboarding path — typing dots
     * only, no owner bubble — and render (and speak) just the reply.
     */
    private fun kickInterview() {
        showTyping()
        setBusy(true)
        ServerClient.EXECUTOR.execute {
            val resp = ServerClient.onboardingMessage(this, "hola")
            runOnUiThread {
                if (resp == null) {
                    setBusy(false)
                    hideTyping()
                    setVoiceStatus(VoiceState.IDLE)
                    // Keep the chat alive even offline: canned hint + next step.
                    addParsed(getString(R.string.onboarding_resume_hint))
                    addMsg(Role.SYSTEM, "⚠️ ${getString(R.string.server_error)}")
                } else {
                    handleAgentResponse(resp, retryTarget = null)
                }
            }
        }
    }

    // ------------------------------------------------- done sheet + permission

    private fun hasNotificationAccess(): Boolean {
        val cn = ComponentName(this, AgenteNotificationListener::class.java)
        val enabled = Settings.Secure.getString(contentResolver, "enabled_notification_listeners")
        return enabled?.split(":")?.any { ComponentName.unflattenFromString(it) == cn } == true
    }

    private fun isBatteryExempt(): Boolean =
        (getSystemService(POWER_SERVICE) as android.os.PowerManager)
            .isIgnoringBatteryOptimizations(packageName)

    private var batteryAsked = false

    /** System dialog: "Allow agente to always run in background?" */
    @android.annotation.SuppressLint("BatteryLife")
    private fun requestBatteryExemption() {
        batteryAsked = true
        try {
            startActivity(
                Intent(Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS,
                       Uri.parse("package:$packageName"))
            )
        } catch (_: Exception) {
            // Some OEM ROMs hide this screen; nothing more we can do here.
        }
    }

    /** Celebration sheet: voice keeps reading; the tap gates on permission. */
    private fun showDoneSheet() {
        val sheet = findViewById<View>(R.id.done_sheet)
        val arrow = findViewById<TextView>(R.id.done_arrow)
        sheet.visibility = View.VISIBLE
        sheet.translationY = 500f
        sheet.animate().translationY(0f).setDuration(380)
            .setInterpolator(android.view.animation.DecelerateInterpolator()).start()

        val pulse = android.animation.ObjectAnimator.ofPropertyValuesHolder(
            arrow,
            android.animation.PropertyValuesHolder.ofFloat(View.SCALE_X, 1f, 1.35f),
            android.animation.PropertyValuesHolder.ofFloat(View.SCALE_Y, 1f, 1.35f)
        ).apply {
            duration = 550
            repeatCount = android.animation.ObjectAnimator.INFINITE
            repeatMode = android.animation.ObjectAnimator.REVERSE
            start()
        }
        sheet.setOnClickListener {
            when {
                !hasNotificationAccess() -> {
                    pendingDone = true
                    MaterialAlertDialogBuilder(this)
                        .setTitle(R.string.permission_title)
                        .setMessage(R.string.permission_explainer)
                        .setPositiveButton(R.string.permission_open_settings) { _, _ ->
                            startActivity(Intent(Settings.ACTION_NOTIFICATION_LISTENER_SETTINGS))
                        }
                        .setNegativeButton(android.R.string.cancel, null)
                        .show()
                }
                !isBatteryExempt() && !batteryAsked -> {
                    pendingDone = true
                    requestBatteryExemption()
                }
                else -> {
                    pulse.cancel()
                    goToDashboard()
                }
            }
        }
    }

    override fun onResume() {
        super.onResume()
        // Back from a permission screen: chain the next step or finish setup.
        if (!pendingDone) return
        if (!hasNotificationAccess()) return // still waiting on the user
        Prefs.setEnabled(this, true)
        if (!isBatteryExempt() && !batteryAsked) {
            requestBatteryExemption()
            return
        }
        // Battery exemption is best-effort: proceed even if declined.
        goToDashboard()
    }

    private fun goToDashboard() {
        // Setup is complete: the agent must actually be on.
        if (hasNotificationAccess()) Prefs.setEnabled(this, true)
        player?.release(); player = null
        // Last two steps: which apps the agent reads, then which of those it
        // answers on — decided on the phone, once, right after the interview.
        val next = if (Prefs.appsSetupDone(this)) DashboardActivity::class.java else {
            Prefs.setAppsSetupPending(this)
            ReadAppsActivity::class.java
        }
        startActivity(Intent(this, next))
        overridePendingTransition(android.R.anim.fade_in, android.R.anim.fade_out)
        finish()
    }

    override fun onPause() {
        super.onPause()
        // Never keep the mic open in the background; the loop resumes on the
        // next agent reply (or an explicit tap) when the owner comes back.
        cancelVoiceListening()
        voiceHandler.removeCallbacksAndMessages(null)
    }

    override fun onDestroy() {
        super.onDestroy()
        timerHandler.removeCallbacksAndMessages(null)
        voiceHandler.removeCallbacksAndMessages(null)
        recordPulse?.cancel()
        statusPulse?.cancel()
        vadRecorder?.release(); vadRecorder = null
        recorder?.release()
        player?.release()
    }

    // ---------------------------------------------------------------- adapter

    private inner class ChatAdapter : RecyclerView.Adapter<RecyclerView.ViewHolder>() {

        private val TYPE_OWNER = 0
        private val TYPE_AGENT = 1
        private val TYPE_SYSTEM = 2
        private val TYPE_TYPING = 3

        override fun getItemCount() = msgs.size + if (typing) 1 else 0

        override fun getItemViewType(position: Int): Int {
            if (typing && position == msgs.size) return TYPE_TYPING
            return when (msgs[position].role) {
                Role.OWNER -> TYPE_OWNER
                Role.AGENT -> TYPE_AGENT
                Role.SYSTEM -> TYPE_SYSTEM
            }
        }

        /** WhatsApp-style tail: full 18dp radius, one sharp 4dp corner on the sender's side. */
        private fun bubbleBg(fillColor: Int, sharpBottomRight: Boolean): MaterialShapeDrawable {
            val r = resources.getDimension(R.dimen.corner_bubble)
            val sharp = resources.getDimension(R.dimen.space_xs)
            val shape = ShapeAppearanceModel.builder()
                .setAllCornerSizes(r)
                .apply {
                    if (sharpBottomRight) setBottomRightCornerSize(sharp)
                    else setBottomLeftCornerSize(sharp)
                }
                .build()
            return MaterialShapeDrawable(shape).apply {
                setTint(ContextCompat.getColor(this@OnboardingActivity, fillColor))
            }
        }

        private val bubbleMaxWidth: Int
            get() = (resources.displayMetrics.widthPixels * 0.78f).toInt()

        override fun onCreateViewHolder(parent: ViewGroup, viewType: Int): RecyclerView.ViewHolder {
            val inf = LayoutInflater.from(parent.context)
            return when (viewType) {
                TYPE_OWNER -> OwnerVH(inf.inflate(R.layout.item_chat_owner, parent, false))
                TYPE_AGENT -> AgentVH(inf.inflate(R.layout.item_chat_agent, parent, false))
                TYPE_TYPING -> TypingVH(inf.inflate(R.layout.item_chat_typing, parent, false))
                else -> SystemVH(inf.inflate(R.layout.item_chat_system, parent, false))
            }
        }

        override fun onBindViewHolder(holder: RecyclerView.ViewHolder, position: Int) {
            when (holder) {
                is OwnerVH -> holder.bind(msgs[position])
                is AgentVH -> holder.bind(msgs[position])
                is SystemVH -> holder.bind(msgs[position])
                is TypingVH -> Unit // animation runs on attach
            }
        }

        override fun onViewAttachedToWindow(holder: RecyclerView.ViewHolder) {
            if (holder is TypingVH) holder.startDots()
        }

        override fun onViewDetachedFromWindow(holder: RecyclerView.ViewHolder) {
            if (holder is TypingVH) holder.stopDots()
        }

        inner class OwnerVH(v: View) : RecyclerView.ViewHolder(v) {
            private val text: TextView = v.findViewById(R.id.bubble_text)
            private val retry: MaterialButton = v.findViewById(R.id.bubble_retry)

            init {
                text.maxWidth = bubbleMaxWidth
                retry.setOnClickListener {
                    val pos = bindingAdapterPosition
                    if (pos != RecyclerView.NO_POSITION && pos < msgs.size) resend(msgs[pos])
                }
            }

            fun bind(m: Msg) {
                // Pending = live speech partial: muted fill + muted text until
                // the recognizer's final result solidifies the bubble.
                text.background = bubbleBg(
                    if (m.pending) R.color.agente_surface_variant
                    else R.color.agente_primary_container,
                    sharpBottomRight = true
                )
                text.setTextColor(
                    ContextCompat.getColor(
                        this@OnboardingActivity,
                        if (m.pending) R.color.agente_on_surface_muted
                        else R.color.agente_on_primary_container
                    )
                )
                text.text = m.text
                retry.visibility = if (m.failed && !m.pending) View.VISIBLE else View.GONE
            }
        }

        inner class AgentVH(v: View) : RecyclerView.ViewHolder(v) {
            private val text: TextView = v.findViewById(R.id.bubble_text)

            init {
                text.background = bubbleBg(R.color.agente_surface_variant, sharpBottomRight = false)
                text.maxWidth = bubbleMaxWidth
            }

            fun bind(m: Msg) { text.text = m.text }
        }

        inner class SystemVH(v: View) : RecyclerView.ViewHolder(v) {
            private val text: TextView = v.findViewById(R.id.system_text)
            fun bind(m: Msg) { text.text = m.text }
        }

        inner class TypingVH(v: View) : RecyclerView.ViewHolder(v) {
            private val dots = listOf<TextView>(
                v.findViewById(R.id.typing_dot1),
                v.findViewById(R.id.typing_dot2),
                v.findViewById(R.id.typing_dot3)
            )
            private val animators = mutableListOf<ObjectAnimator>()

            init {
                v.findViewById<LinearLayout>(R.id.typing_bubble).background =
                    bubbleBg(R.color.agente_surface_variant, sharpBottomRight = false)
            }

            fun startDots() {
                stopDots()
                dots.forEachIndexed { i, dot ->
                    animators += ObjectAnimator.ofFloat(dot, View.ALPHA, 0.25f, 1f).apply {
                        duration = 450
                        startDelay = i * 160L
                        repeatCount = ObjectAnimator.INFINITE
                        repeatMode = ObjectAnimator.REVERSE
                        start()
                    }
                }
            }

            fun stopDots() {
                animators.forEach { it.cancel() }
                animators.clear()
                dots.forEach { it.alpha = 0.25f }
            }
        }
    }
}
