package tech.yaya.agente

import android.content.ClipData
import android.content.ClipboardManager
import android.content.ComponentName
import android.content.Intent
import android.content.res.ColorStateList
import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.graphics.Matrix
import android.net.Uri
import android.os.Bundle
import android.provider.MediaStore
import android.provider.Settings
import android.text.format.DateUtils
import android.util.LruCache
import android.view.Menu
import android.view.View
import android.view.ViewGroup
import android.widget.ImageButton
import android.widget.ImageView
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.TextView
import android.widget.Toast
import androidx.appcompat.app.AppCompatActivity
import androidx.core.content.FileProvider
import androidx.exifinterface.media.ExifInterface
import androidx.swiperefreshlayout.widget.SwipeRefreshLayout
import com.google.android.material.bottomnavigation.BottomNavigationView
import com.google.android.material.card.MaterialCardView
import com.google.android.material.chip.Chip
import com.google.android.material.dialog.MaterialAlertDialogBuilder
import com.google.android.material.materialswitch.MaterialSwitch
import com.google.android.material.snackbar.Snackbar
import org.json.JSONObject
import java.io.ByteArrayInputStream
import java.io.ByteArrayOutputStream
import java.io.File

/**
 * The owner's home (DECISIONS D15): the header and the truth chip are
 * fixed; everything under them is drawn from the UI spec the core composed
 * for THIS business — tabs in the bottom bar, blocks from [Blocks] in each.
 * A restaurant opens on its pedidos board, a salon on today's citas, a
 * Gamarra stall on its catalog; the agent named the tabs during onboarding
 * and can redraw them whenever the owner asks.
 */
class DashboardActivity : AppCompatActivity() {

    private lateinit var businessName: TextView
    private lateinit var statusChip: Chip
    private lateinit var agentSwitch: MaterialSwitch
    private lateinit var offBanner: View
    private lateinit var content: LinearLayout
    private lateinit var banners: View
    private lateinit var nav: BottomNavigationView
    private lateinit var scroll: ScrollView
    private lateinit var swipe: SwipeRefreshLayout

    /** Last dashboard payload rendered (network or cache). */
    private var data: JSONObject? = null
    private var spec: UiSpec? = null
    private var navSignature = ""
    private var currentTab = 0
    private var walkthrough: Walkthrough? = null

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(R.layout.activity_dashboard)
        businessName = findViewById(R.id.dash_business_name)
        statusChip = findViewById(R.id.dash_status)
        agentSwitch = findViewById(R.id.dash_agent_switch)
        offBanner = findViewById(R.id.off_banner)
        content = findViewById(R.id.tab_content)
        banners = findViewById(R.id.dash_banners)
        nav = findViewById(R.id.dash_nav)
        scroll = findViewById(R.id.dash_scroll)
        swipe = findViewById(R.id.dash_swipe)

        findViewById<ImageButton>(R.id.dash_settings).setOnClickListener {
            startActivity(Intent(this, MainActivity::class.java))
            overridePendingTransition(android.R.anim.fade_in, android.R.anim.fade_out)
        }
        // "Habla con tu negocio": the onboarding chat lives on after setup as
        // the owner's management console — same voice, camera and text, now
        // wired to the server's manager agent.
        findViewById<TextView>(R.id.dash_chat).setOnClickListener {
            startActivity(Intent(this, OnboardingActivity::class.java))
            overridePendingTransition(android.R.anim.fade_in, android.R.anim.fade_out)
        }
        findViewById<View>(R.id.credits_button).setOnClickListener { startActivity(Intent(this, CreditsActivity::class.java)) }
        findViewById<View>(R.id.off_banner_button).setOnClickListener { activateAgent() }
        findViewById<View>(R.id.ossync_enable).setOnClickListener {
            Prefs.setOsSyncOffered(this)
            requestPermissions(OsSync.CONTACT_PERMS + OsSync.CALENDAR_PERMS, OsSync.RC_CONTACTS)
        }
        findViewById<View>(R.id.ossync_later).setOnClickListener { Prefs.setOsSyncOffered(this); findViewById<View>(R.id.ossync_banner).visibility = View.GONE }
        agentSwitch.setOnCheckedChangeListener { btn, on ->
            if (!btn.isPressed) return@setOnCheckedChangeListener
            if (on && !hasNotificationAccess()) {
                agentSwitch.isChecked = false
                startActivity(Intent(Settings.ACTION_NOTIFICATION_LISTENER_SETTINGS))
            } else {
                Prefs.setEnabled(this, on)
                refreshStatus(lastFetchOk)
            }
        }
        swipe.setOnRefreshListener { load() }
        nav.setOnItemSelectedListener { item ->
            if (item.itemId != currentTab) { currentTab = item.itemId; renderTab(); scroll.scrollTo(0, 0) }
            true
        }

        // Render cache instantly so the screen never opens blank.
        Prefs.dashboardCache(this)?.let { runCatching { render(JSONObject(it)) } }
    }

    private var lastFetchOk = true

    override fun onResume() {
        super.onResume()
        refreshStatus(lastFetchOk)
        load()
        maybeAskBatteryExemption()
        maybeAskNotifPermission()
    }

    override fun onRequestPermissionsResult(requestCode: Int, permissions: Array<out String>, grantResults: IntArray) {
        super.onRequestPermissionsResult(requestCode, permissions, grantResults)
        if (requestCode != OsSync.RC_CONTACTS) return
        val ok = { names: Array<String> -> names.all { p -> permissions.indexOf(p).let { i -> i >= 0 && grantResults[i] == android.content.pm.PackageManager.PERMISSION_GRANTED } } }
        Prefs.setSyncContacts(this, ok(OsSync.CONTACT_PERMS))
        Prefs.setSyncCalendar(this, ok(OsSync.CALENDAR_PERMS))
        if (!ok(OsSync.CONTACT_PERMS) && !ok(OsSync.CALENDAR_PERMS)) Toast.makeText(this, R.string.ossync_permission_denied, Toast.LENGTH_LONG).show()
        findViewById<View>(R.id.ossync_banner).visibility = View.GONE
        OsSync.syncAll(this)
    }

    /** Export the upcoming agenda as a calendar file anyone can import. */
    fun exportIcs() {
        ServerClient.IO_EXECUTOR.execute {
            val rows = ServerClient.appointments(this)
            runOnUiThread {
                if (rows == null || rows.length() == 0) { Toast.makeText(this, R.string.export_nothing, Toast.LENGTH_SHORT).show(); return@runOnUiThread }
                OsSync.shareFile(this, "agenda.ics", "text/calendar", OsSync.ics(rows))
            }
        }
    }

    /** Android 13+: owner alerts (customer needs you) require this grant. */
    private fun maybeAskNotifPermission() {
        if (android.os.Build.VERSION.SDK_INT < 33 || notifAskedThisSession) return
        if (checkSelfPermission(android.Manifest.permission.POST_NOTIFICATIONS) ==
            android.content.pm.PackageManager.PERMISSION_GRANTED
        ) return
        notifAskedThisSession = true
        requestPermissions(arrayOf(android.Manifest.permission.POST_NOTIFICATIONS), 7)
    }

    /**
     * Xiaomi/Huawei-class ROMs kill background listeners under battery
     * optimization; ask once per session for the exemption so the agent
     * survives overnight.
     */
    @android.annotation.SuppressLint("BatteryLife")
    private fun maybeAskBatteryExemption() {
        if (batteryAskedThisSession) return
        val pm = getSystemService(POWER_SERVICE) as android.os.PowerManager
        if (pm.isIgnoringBatteryOptimizations(packageName)) return
        if (!hasNotificationAccess() || !Prefs.isEnabled(this)) return
        batteryAskedThisSession = true
        try {
            startActivity(Intent(Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS, Uri.parse("package:$packageName")))
        } catch (_: Exception) {
        }
    }

    companion object {
        private var batteryAskedThisSession = false
        private var notifAskedThisSession = false
        private const val RC_PHOTO_CAMERA = 41
        private const val RC_PHOTO_GALLERY = 42
        private const val MAX_PHOTO_EDGE = 1280
    }

    private fun hasNotificationAccess(): Boolean {
        val cn = ComponentName(this, AgenteNotificationListener::class.java)
        val enabled = Settings.Secure.getString(contentResolver, "enabled_notification_listeners")
        return enabled?.split(":")?.any { ComponentName.unflattenFromString(it) == cn } == true
    }

    private fun activateAgent() {
        if (!hasNotificationAccess()) {
            startActivity(Intent(Settings.ACTION_NOTIFICATION_LISTENER_SETTINGS))
            return
        }
        Prefs.setEnabled(this, true)
        refreshStatus(lastFetchOk)
    }

    /** Last credits summary from the core (`dashboard.credits`); null until the first good fetch. */
    private var credits: JSONObject? = null

    /** Tint an informational chip from color tokens. */
    fun styleChip(chip: Chip, bgRes: Int, fgRes: Int) {
        chip.chipBackgroundColor = ColorStateList.valueOf(getColor(bgRes))
        chip.setTextColor(getColor(fgRes))
    }

    fun selectableBorderless(): android.graphics.drawable.Drawable? {
        val ta = obtainStyledAttributes(intArrayOf(android.R.attr.selectableItemBackgroundBorderless))
        return try { ta.getDrawable(0) } finally { ta.recycle() }
    }

    /**
     * Credits banner: visible once the core reported a balance. Green with
     * the balance and prices; amber below USD 2 ("Recarga pronto"); red in
     * the negative grace window; red "modo manual" past it, when the agent
     * has stopped replying and hands conversations to the owner. The
     * balance never decides who gets served (docs/CREDITS.md § 2).
     */
    private fun refreshCreditsBanner() {
        // D17: offered once, after the interview, until the owner decides.
        findViewById<View>(R.id.ossync_banner).visibility =
            if (!Prefs.osSyncOffered(this) && data?.optBoolean("onboarded") == true && !(Prefs.syncContacts(this) && Prefs.syncCalendar(this))) View.VISIBLE else View.GONE
        val c = credits
        val banner = findViewById<MaterialCardView>(R.id.credits_banner)
        if (c == null) { banner.visibility = View.GONE; return }
        banner.visibility = View.VISIBLE
        val text = findViewById<TextView>(R.id.credits_banner_text)
        val bal = Credits.money(c, Credits.balance(c))
        val (line, red) = when (Credits.state(c)) {
            Credits.State.MANUAL -> getString(R.string.credits_banner_manual, bal) to true
            Credits.State.GRACE -> getString(R.string.credits_banner_grace, bal) to true
            Credits.State.LOW -> getString(R.string.credits_banner_low, bal) to false
            else -> getString(R.string.credits_banner_ok, bal, Credits.money(c, Credits.priceKnown(c)), Credits.money(c, Credits.priceNew(c))) to false
        }
        val deposits = if (Credits.depositsEnabled(c)) "" else "\n" + getString(R.string.credits_deposits_soon)
        text.text = line + deposits
        val amber = Credits.state(c) == Credits.State.LOW
        text.setTextColor(getColor(when { red -> R.color.agente_error; else -> R.color.agente_on_secondary_container }))
        banner.setCardBackgroundColor(getColor(when { red -> R.color.agente_error_container; amber -> R.color.agente_warning_container; else -> R.color.agente_secondary_container }))
    }

    /** The truth chip: is the agent actually able to answer right now? */
    private fun refreshStatus(fetchOk: Boolean) {
        val alive = hasNotificationAccess() && Prefs.isEnabled(this)
        agentSwitch.isChecked = alive
        offBanner.visibility = if (alive) View.GONE else View.VISIBLE
        refreshCreditsBanner()
        when {
            !alive -> {
                statusChip.text = getString(R.string.status_off_banner)
                styleChip(statusChip, R.color.agente_error_container, R.color.agente_error)
            }
            !fetchOk -> {
                statusChip.text = getString(R.string.dash_offline_chip)
                styleChip(statusChip, R.color.agente_secondary_container, R.color.agente_on_secondary_container)
            }
            Credits.manual(credits) -> {
                statusChip.text = getString(R.string.status_manual_mode)
                styleChip(statusChip, R.color.agente_error_container, R.color.agente_error)
            }
            else -> {
                val lastReply = ReplyLog.load(this).firstOrNull { it.replySent }
                statusChip.text = if (lastReply != null) getString(
                    R.string.status_active_last, DateUtils.getRelativeTimeSpanString(lastReply.timestamp).toString()
                ) else getString(R.string.status_active)
                styleChip(statusChip, R.color.agente_primary_container, R.color.agente_on_primary_container)
            }
        }
    }

    // ---------------------------------------------------------------- data

    private fun load() {
        swipe.isRefreshing = true
        ServerClient.IO_EXECUTOR.execute {
            val d = ServerClient.dashboard(this)
            runOnUiThread {
                if (isFinishing) return@runOnUiThread
                swipe.isRefreshing = false
                lastFetchOk = d != null
                if (d != null) {
                    Prefs.setDashboardCache(this, d.toString())
                    ServerClient.IO_EXECUTOR.execute { runCatching { Prefs.refreshLearnedSources(applicationContext) } }
                    render(d)
                    // D17: the OS copies follow every refresh (idempotent, off-thread).
                    OsSync.syncAll(this)
                } else {
                    Prefs.dashboardCache(this)?.let { runCatching { render(JSONObject(it)) } }
                }
                refreshStatus(lastFetchOk)
                loadConversations()
            }
        }
    }

    /** Re-fetch after an action (a swipe, a photo) without the spinner dance. */
    fun reload() = load()

    fun soles(v: Double): String = Prefs.money(this, v)

    fun inflateIn(layoutRes: Int, parent: ViewGroup): View = layoutInflater.inflate(layoutRes, parent, false)

    /** Teaching empty state ("what will appear here, and how to make it happen"). */
    fun emptyState(parent: ViewGroup, msg: String): View =
        inflateIn(R.layout.item_dash_empty, parent).apply { findViewById<TextView>(R.id.dash_empty_text).text = msg }

    private fun render(d: JSONObject) {
        d.optJSONObject("locale")?.let { Prefs.setLocale(this, it) }
        businessName.text = d.optString("businessName", getString(R.string.app_name))
        credits = d.optJSONObject("credits")
        data = d
        val s = UiSpec.parse(d.optJSONObject("ui")) ?: UiSpec.fallback(this, d.optString("businessKind"))
        val redesigned = spec != null && s.signature() != navSignature
        spec = s
        if (s.signature() != navSignature) {
            navSignature = s.signature()
            nav.menu.clear()
            s.tabs.forEachIndexed { i, t -> nav.menu.add(Menu.NONE, i, i, t.label).setIcon(UiSpec.iconRes(t.icon)) }
            nav.visibility = if (s.tabs.size > 1) View.VISIBLE else View.GONE
            currentTab = s.homeIndex().coerceIn(0, s.tabs.size - 1)
            nav.selectedItemId = currentTab
            if (redesigned && walkthrough?.isShowing() != true) {
                Snackbar.make(swipe, R.string.ui_redesigned, Snackbar.LENGTH_LONG).show()
            }
        }
        renderTab()
        maybeWalkthrough(d)
    }

    private fun renderTab() {
        val s = spec ?: return
        val d = data ?: return
        val tab = s.tabs.getOrNull(currentTab) ?: return
        banners.visibility = if (currentTab == s.homeIndex()) View.VISIBLE else View.GONE
        content.removeAllViews()
        for (b in tab.blocks) Blocks.render(this, b, content, d)
    }

    private fun selectTab(i: Int) {
        if (i !in (spec?.tabs?.indices ?: IntRange.EMPTY)) return
        currentTab = i
        nav.selectedItemId = i
        renderTab()
        scroll.scrollTo(0, 0)
    }

    /** Once, the first time the designed app opens: a step per tab. */
    private fun maybeWalkthrough(d: JSONObject) {
        val s = spec ?: return
        if (walkthrough?.isShowing() == true) return
        if (!d.optBoolean("onboarded", false) || Prefs.walkthroughSeen(this)) return
        walkthrough = Walkthrough(this, s, ::selectTab) { Prefs.setWalkthroughSeen(this, true) }.also { it.show() }
    }

    var conversations: org.json.JSONArray? = null
        private set

    private fun loadConversations() {
        ServerClient.IO_EXECUTOR.execute {
            val v = ServerClient.conversations(this)?.optJSONArray("conversations")
            runOnUiThread {
                if (isFinishing) return@runOnUiThread
                conversations = v
                if (spec?.tabs?.getOrNull(currentTab)?.blocks?.any { it.type == "conversations" } == true) renderTab()
            }
        }
    }

    // ------------------------------------------------------------ attention

    fun askGapAnswer(gapId: String, question: String) {
        val input = android.widget.EditText(this).apply { hint = getString(R.string.gap_answer_hint); minLines = 2 }
        val pad = resources.getDimensionPixelSize(R.dimen.space_l)
        val wrap = android.widget.FrameLayout(this).apply { setPadding(pad, pad / 2, pad, 0); addView(input) }
        MaterialAlertDialogBuilder(this)
            .setTitle(question)
            .setView(wrap)
            .setNegativeButton(android.R.string.cancel, null)
            .setPositiveButton(R.string.gap_answer_send) { _, _ ->
                val answer = input.text.toString().trim()
                if (answer.isEmpty()) return@setPositiveButton
                ServerClient.IO_EXECUTOR.execute {
                    val res = ServerClient.answerGap(this, gapId, answer)
                    runOnUiThread {
                        Toast.makeText(this, if (res != null) getString(R.string.gap_answered) else getString(R.string.gap_answer_failed), Toast.LENGTH_LONG).show()
                        if (res != null) load()
                    }
                }
            }
            .show()
    }

    // ------------------------------------------------------------ the queue

    /** The swipe: done now, undo for a few seconds, then the truth from the core. */
    fun markDone(isOrder: Boolean, item: JSONObject, undo: () -> Unit) {
        val id = item.optString("id")
        var undone = false
        ServerClient.IO_EXECUTOR.execute {
            val r = if (isOrder) ServerClient.orderStatus(this, id, "done") else ServerClient.appointmentStatus(this, id, "done")
            if (r == null) runOnUiThread { undo(); Toast.makeText(this, R.string.queue_failed, Toast.LENGTH_LONG).show() }
        }
        Snackbar.make(swipe, getString(R.string.queue_marked_done, item.optString("customer")), Snackbar.LENGTH_LONG)
            .setAction(R.string.queue_undo) {
                undone = true
                undo()
                ServerClient.IO_EXECUTOR.execute {
                    if (isOrder) ServerClient.orderStatus(this, id, "undo") else ServerClient.appointmentStatus(this, id, "undo")
                    runOnUiThread { load() }
                }
            }
            .addCallback(object : Snackbar.Callback() {
                override fun onDismissed(sb: Snackbar?, event: Int) { if (!undone) load() }
            })
            .show()
    }

    private fun setStatus(isOrder: Boolean, id: String, status: String) {
        ServerClient.IO_EXECUTOR.execute {
            val r = if (isOrder) ServerClient.orderStatus(this, id, status) else ServerClient.appointmentStatus(this, id, status)
            runOnUiThread {
                if (r == null) Toast.makeText(this, R.string.queue_failed, Toast.LENGTH_LONG).show()
                load()
            }
        }
    }

    fun orderActions(o: JSONObject) {
        val done = o.optString("status") == "done"
        val paid = o.optBoolean("paid")
        val items = ArrayList<Pair<String, String>>()
        if (done) items.add(getString(R.string.queue_undo_done) to "undo") else items.add(getString(R.string.queue_done_action) to "done")
        if (!paid) items.add(getString(R.string.queue_mark_paid) to "paid")
        items.add(getString(R.string.queue_cancel_order) to "cancelled")
        MaterialAlertDialogBuilder(this)
            .setTitle(o.optString("customer") + " · " + soles(o.optDouble("total", 0.0)))
            .setItems(items.map { it.first }.toTypedArray()) { _, i -> setStatus(true, o.optString("id"), items[i].second) }
            .setNegativeButton(android.R.string.cancel, null)
            .show()
    }

    fun appointmentActions(a: JSONObject) {
        val status = a.optString("status")
        val items = ArrayList<Pair<String, String>>()
        if (status == "done" || status == "no_show") items.add(getString(R.string.queue_undo_done) to "undo") else {
            items.add(getString(R.string.queue_done_action) to "done")
            items.add(getString(R.string.queue_no_show_action) to "no_show")
        }
        if (!a.optBoolean("paid")) items.add(getString(R.string.queue_mark_paid) to "paid")
        items.add(getString(R.string.queue_cancel_appt) to "cancelled")
        MaterialAlertDialogBuilder(this)
            .setTitle(a.optString("time") + " · " + a.optString("customer"))
            .setMessage(listOfNotNull(
                a.optString("service").takeIf { it.isNotBlank() && it != "null" },
                a.optString("specialist").takeIf { it.isNotBlank() && it != "null" },
                Blocks.prettyDate(a.optString("date")),
            ).joinToString(" · "))
            .setItems(items.map { it.first }.toTypedArray()) { _, i -> setStatus(false, a.optString("id"), items[i].second) }
            .setNegativeButton(android.R.string.cancel, null)
            .show()
    }

    // -------------------------------------------------------------- photos

    private val photoCache = LruCache<String, Bitmap>(24)
    private val photoFile: File by lazy { File(cacheDir, "catalog.jpg") }
    private var pendingProduct: String? = null

    fun loadPhoto(id: String, iv: ImageView) {
        iv.tag = id
        photoCache.get(id)?.let { iv.setImageBitmap(it); return }
        iv.setImageDrawable(null)
        ServerClient.IO_EXECUTOR.execute {
            val bytes = ServerClient.mediaBytes(this, id) ?: return@execute
            val bounds = BitmapFactory.Options().apply { inJustDecodeBounds = true }
            BitmapFactory.decodeByteArray(bytes, 0, bytes.size, bounds)
            var sample = 1
            while (maxOf(bounds.outWidth, bounds.outHeight) / (sample * 2) >= 480) sample *= 2
            val bmp = BitmapFactory.decodeByteArray(bytes, 0, bytes.size, BitmapFactory.Options().apply { inSampleSize = sample }) ?: return@execute
            photoCache.put(id, bmp)
            runOnUiThread { if (iv.tag == id) iv.setImageBitmap(bmp) }
        }
    }

    /** Camera or gallery; [product] preselects which product the photo belongs to. */
    fun pickPhotoFor(product: String?) {
        pendingProduct = product
        MaterialAlertDialogBuilder(this)
            .setTitle(product?.let { getString(R.string.catalog_photo_for, it) } ?: getString(R.string.catalog_add_photo))
            .setItems(arrayOf(getString(R.string.catalog_photo_take), getString(R.string.catalog_photo_gallery))) { _, which ->
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
            .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_GRANT_WRITE_URI_PERMISSION)
        intent.clipData = ClipData.newRawUri("output", uri)
        try { @Suppress("DEPRECATION") startActivityForResult(intent, RC_PHOTO_CAMERA) } catch (_: Exception) {
            Toast.makeText(this, getString(R.string.catalog_photo_no_app), Toast.LENGTH_LONG).show()
        }
    }

    private fun launchGallery() {
        val intent = Intent(Intent.ACTION_GET_CONTENT).setType("image/*").addCategory(Intent.CATEGORY_OPENABLE)
        try { @Suppress("DEPRECATION") startActivityForResult(intent, RC_PHOTO_GALLERY) } catch (_: Exception) {
            Toast.makeText(this, getString(R.string.catalog_photo_no_app), Toast.LENGTH_LONG).show()
        }
    }

    @Deprecated("Deprecated in Java")
    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        @Suppress("DEPRECATION") super.onActivityResult(requestCode, resultCode, data)
        if (resultCode != RESULT_OK) return
        val raw: ByteArray? = when (requestCode) {
            RC_PHOTO_CAMERA -> photoFile.takeIf { it.exists() && it.length() > 0 }?.readBytes()
            RC_PHOTO_GALLERY -> data?.data?.let { uri -> runCatching { contentResolver.openInputStream(uri)?.use { it.readBytes() } }.getOrNull() }
            else -> return
        }
        if (raw == null || raw.isEmpty()) { Toast.makeText(this, R.string.catalog_photo_unreadable, Toast.LENGTH_LONG).show(); return }
        val product = pendingProduct
        if (product != null) uploadPhoto(raw, product) else askProductThen(raw)
    }

    /** Which product is this? The catalog's names first, or a new one. */
    private fun askProductThen(raw: ByteArray) {
        val names = this.data?.optJSONObject("products")?.keys()?.asSequence()?.toList()?.sorted() ?: emptyList()
        val other = getString(R.string.catalog_photo_other_product)
        val options = (names + other).toTypedArray()
        MaterialAlertDialogBuilder(this)
            .setTitle(R.string.catalog_photo_product_hint)
            .setItems(options) { _, i ->
                if (i < names.size) uploadPhoto(raw, names[i]) else {
                    val input = android.widget.EditText(this).apply { hint = getString(R.string.catalog_photo_product_name) }
                    val pad = resources.getDimensionPixelSize(R.dimen.space_l)
                    val wrap = android.widget.FrameLayout(this).apply { setPadding(pad, pad / 2, pad, 0); addView(input) }
                    MaterialAlertDialogBuilder(this).setTitle(R.string.catalog_photo_product_hint).setView(wrap)
                        .setPositiveButton(android.R.string.ok) { _, _ -> uploadPhoto(raw, input.text.toString().trim().ifEmpty { null }) }
                        .setNegativeButton(R.string.catalog_photo_skip_product) { _, _ -> uploadPhoto(raw, null) }
                        .show()
                }
            }
            .setNegativeButton(android.R.string.cancel, null)
            .show()
    }

    private fun uploadPhoto(raw: ByteArray, product: String?) {
        Toast.makeText(this, R.string.catalog_uploading, Toast.LENGTH_SHORT).show()
        ServerClient.IO_EXECUTOR.execute {
            val jpeg = prepareJpeg(raw)
            val r = jpeg?.let { ServerClient.mediaUpload(this, it, product, null) }
            runOnUiThread {
                if (r == null || r.code !in 200..299) Toast.makeText(this, R.string.catalog_upload_failed, Toast.LENGTH_LONG).show()
                else { Toast.makeText(this, R.string.catalog_uploaded, Toast.LENGTH_SHORT).show(); load() }
            }
        }
    }

    /** Same treatment as the onboarding catalog photo: ≤1280px long edge, EXIF-upright, JPEG 85. */
    private fun prepareJpeg(raw: ByteArray): ByteArray? = try {
        val bounds = BitmapFactory.Options().apply { inJustDecodeBounds = true }
        BitmapFactory.decodeByteArray(raw, 0, raw.size, bounds)
        if (bounds.outWidth <= 0 || bounds.outHeight <= 0) null else {
            var sample = 1
            while (maxOf(bounds.outWidth, bounds.outHeight) / (sample * 2) >= MAX_PHOTO_EDGE) sample *= 2
            var bmp = BitmapFactory.decodeByteArray(raw, 0, raw.size, BitmapFactory.Options().apply { inSampleSize = sample })!!
            val longest = maxOf(bmp.width, bmp.height)
            if (longest > MAX_PHOTO_EDGE) {
                val scale = MAX_PHOTO_EDGE.toFloat() / longest
                bmp = Bitmap.createScaledBitmap(bmp, (bmp.width * scale).toInt().coerceAtLeast(1), (bmp.height * scale).toInt().coerceAtLeast(1), true)
            }
            val rotation = try {
                when (ExifInterface(ByteArrayInputStream(raw)).getAttributeInt(ExifInterface.TAG_ORIENTATION, ExifInterface.ORIENTATION_NORMAL)) {
                    ExifInterface.ORIENTATION_ROTATE_90 -> 90f; ExifInterface.ORIENTATION_ROTATE_180 -> 180f; ExifInterface.ORIENTATION_ROTATE_270 -> 270f; else -> 0f
                }
            } catch (_: Exception) { 0f }
            if (rotation != 0f) bmp = Bitmap.createBitmap(bmp, 0, 0, bmp.width, bmp.height, Matrix().apply { postRotate(rotation) }, true)
            ByteArrayOutputStream().also { bmp.compress(Bitmap.CompressFormat.JPEG, 85, it) }.toByteArray()
        }
    } catch (e: Exception) { android.util.Log.w("Dashboard", "photo decode failed", e); null }

    /** A photo, big, with what you can do to it. */
    fun openPhoto(m: JSONObject) {
        val id = m.optString("id")
        val iv = ImageView(this).apply { adjustViewBounds = true; scaleType = ImageView.ScaleType.FIT_CENTER }
        loadPhoto(id, iv)
        val label = m.optString("product").takeIf { it.isNotBlank() && it != "null" } ?: getString(R.string.catalog_photo_no_product)
        MaterialAlertDialogBuilder(this)
            .setTitle(label)
            .setView(iv)
            .setPositiveButton(R.string.catalog_share) { _, _ -> shareLink(listOf(id), emptyList()) }
            .setNeutralButton(R.string.catalog_change_product) { _, _ -> renamePhoto(m) }
            .setNegativeButton(R.string.catalog_delete) { _, _ ->
                ServerClient.IO_EXECUTOR.execute {
                    val ok = ServerClient.mediaDelete(this, id)
                    runOnUiThread { if (ok) { photoCache.remove(id); load() } else Toast.makeText(this, R.string.queue_failed, Toast.LENGTH_LONG).show() }
                }
            }
            .show()
    }

    private fun renamePhoto(m: JSONObject) {
        val names = this.data?.optJSONObject("products")?.keys()?.asSequence()?.toList()?.sorted() ?: emptyList()
        val input = android.widget.EditText(this).apply { setText(m.optString("product").takeIf { it != "null" }); hint = getString(R.string.catalog_photo_product_name) }
        val pad = resources.getDimensionPixelSize(R.dimen.space_l)
        val wrap = android.widget.FrameLayout(this).apply { setPadding(pad, pad / 2, pad, 0); addView(input) }
        val b = MaterialAlertDialogBuilder(this).setTitle(R.string.catalog_photo_product_hint).setView(wrap)
            .setPositiveButton(android.R.string.ok) { _, _ ->
                val p = input.text.toString().trim()
                ServerClient.IO_EXECUTOR.execute { ServerClient.mediaUpdate(this, m.optString("id"), p, null); runOnUiThread { load() } }
            }
            .setNegativeButton(android.R.string.cancel, null)
        if (names.isNotEmpty()) b.setNeutralButton(R.string.catalog_photo_pick_product) { _, _ ->
            MaterialAlertDialogBuilder(this).setItems(names.toTypedArray()) { _, i ->
                ServerClient.IO_EXECUTOR.execute { ServerClient.mediaUpdate(this, m.optString("id"), names[i], null); runOnUiThread { load() } }
            }.show()
        }
        b.show()
    }

    /** Mint the private link and offer to copy it or send it. */
    fun shareLink(ids: List<String>, products: List<String>) {
        Toast.makeText(this, R.string.catalog_link_creating, Toast.LENGTH_SHORT).show()
        ServerClient.IO_EXECUTOR.execute {
            val r = ServerClient.mediaShare(this, ids, products, null)
            runOnUiThread {
                val url = r.json?.optString("url").takeIf { r.code in 200..299 && !it.isNullOrBlank() }
                if (url == null) {
                    Toast.makeText(this, if (r.code == 404) R.string.catalog_link_nothing else R.string.catalog_link_failed, Toast.LENGTH_LONG).show()
                    return@runOnUiThread
                }
                val n = r.json?.optInt("photos") ?: 0
                MaterialAlertDialogBuilder(this)
                    .setTitle(R.string.catalog_link_title)
                    .setMessage(getString(R.string.catalog_link_body, n, url))
                    .setPositiveButton(R.string.catalog_link_send) { _, _ ->
                        val send = Intent(Intent.ACTION_SEND).setType("text/plain").putExtra(Intent.EXTRA_TEXT, getString(R.string.catalog_link_message, url))
                        startActivity(Intent.createChooser(send, getString(R.string.catalog_link_send)))
                    }
                    .setNeutralButton(R.string.catalog_link_copy) { _, _ ->
                        (getSystemService(CLIPBOARD_SERVICE) as ClipboardManager).setPrimaryClip(ClipData.newPlainText("agente", url))
                        Toast.makeText(this, R.string.catalog_link_copied, Toast.LENGTH_SHORT).show()
                    }
                    .setNegativeButton(android.R.string.cancel, null)
                    .show()
            }
        }
    }
}
