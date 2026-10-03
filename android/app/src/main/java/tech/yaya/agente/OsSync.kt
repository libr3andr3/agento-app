package tech.yaya.agente

import android.Manifest
import android.content.ContentProviderOperation
import android.content.ContentUris
import android.content.ContentValues
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.net.Uri
import android.provider.CalendarContract
import android.provider.ContactsContract
import androidx.core.content.FileProvider
import org.json.JSONArray
import org.json.JSONObject
import java.io.File
import java.text.SimpleDateFormat
import java.util.Locale
import java.util.TimeZone

/**
 * "Tus clientes se quedan contigo" made literal (DECISIONS D17): the CRM is
 * mirrored into the phone's own Contacts and Calendar — data the OS keeps,
 * syncs to the owner's Google account and shows inside WhatsApp — and can be
 * exported as files (`.vcf`, `.csv`, `.ics`) the owner can walk away with.
 *
 * One-way, phone core → OS, idempotent: contacts are tagged with a custom
 * MIME row carrying the CRM id, events with a `CUSTOM_APP_URI`; a second run
 * finds them instead of duplicating. Only customers with a phone number
 * become OS contacts — a WhatsApp notification carries a name, not a number.
 */
object OsSync {
    const val MIME_TAG = "vnd.android.cursor.item/vnd.yaya.agente.customer"
    private const val CALENDAR_NAME = "agente"
    private const val CALENDAR_ACCOUNT = "agente"
    private const val EVENT_URI_PREFIX = "agente://appointment/"
    const val RC_CONTACTS = 71
    const val RC_CALENDAR = 72

    val CONTACT_PERMS = arrayOf(Manifest.permission.READ_CONTACTS, Manifest.permission.WRITE_CONTACTS)
    val CALENDAR_PERMS = arrayOf(Manifest.permission.READ_CALENDAR, Manifest.permission.WRITE_CALENDAR)

    fun hasContacts(ctx: Context) = CONTACT_PERMS.all { ctx.checkSelfPermission(it) == PackageManager.PERMISSION_GRANTED }
    fun hasCalendar(ctx: Context) = CALENDAR_PERMS.all { ctx.checkSelfPermission(it) == PackageManager.PERMISSION_GRANTED }

    /** Everything enabled and permitted, off the main thread. Errors are logged, never thrown. */
    fun syncAll(ctx: Context) {
        ServerClient.IO_EXECUTOR.execute {
            runCatching {
                if (Prefs.syncContacts(ctx) && hasContacts(ctx)) {
                    val rows = ServerClient.contacts(ctx, "")?.optJSONArray("contacts") ?: JSONArray()
                    syncContacts(ctx, rows)
                }
                if (Prefs.syncCalendar(ctx) && hasCalendar(ctx)) {
                    val rows = ServerClient.appointments(ctx) ?: JSONArray()
                    syncCalendar(ctx, rows)
                }
            }.onFailure { android.util.Log.w("OsSync", "sync failed", it) }
        }
    }

    // ------------------------------------------------------------ contacts

    private fun businessLabel(ctx: Context): String =
        Prefs.dashboardCache(ctx)?.let { runCatching { JSONObject(it).optString("businessName") }.getOrNull() }?.takeIf { it.isNotBlank() } ?: ctx.getString(R.string.app_name)

    /** Inserts CRM customers with a phone number that are not in Contacts yet. Returns how many were added. */
    fun syncContacts(ctx: Context, contacts: JSONArray): Int {
        val resolver = ctx.contentResolver
        val note = ctx.getString(R.string.ossync_contact_note, businessLabel(ctx))
        var added = 0
        for (i in 0 until contacts.length()) {
            val c = contacts.getJSONObject(i)
            if (c.optString("kind") == "owner") continue
            val id = c.optString("id"); if (id.isBlank() || id == "null") continue
            val phone = c.optString("phone").filter { it.isDigitOrPlus() }.trimStart('+')
            val name = c.optString("name").takeIf { it.isNotBlank() && it != "null" } ?: continue
            if (phone.length < 7) continue
            if (findRawContact(ctx, id) != null) continue
            val ops = ArrayList<ContentProviderOperation>()
            ops.add(ContentProviderOperation.newInsert(ContactsContract.RawContacts.CONTENT_URI)
                .withValue(ContactsContract.RawContacts.ACCOUNT_TYPE, null)
                .withValue(ContactsContract.RawContacts.ACCOUNT_NAME, null).build())
            ops.add(ContentProviderOperation.newInsert(ContactsContract.Data.CONTENT_URI)
                .withValueBackReference(ContactsContract.Data.RAW_CONTACT_ID, 0)
                .withValue(ContactsContract.Data.MIMETYPE, ContactsContract.CommonDataKinds.StructuredName.CONTENT_ITEM_TYPE)
                .withValue(ContactsContract.CommonDataKinds.StructuredName.DISPLAY_NAME, name).build())
            ops.add(ContentProviderOperation.newInsert(ContactsContract.Data.CONTENT_URI)
                .withValueBackReference(ContactsContract.Data.RAW_CONTACT_ID, 0)
                .withValue(ContactsContract.Data.MIMETYPE, ContactsContract.CommonDataKinds.Phone.CONTENT_ITEM_TYPE)
                .withValue(ContactsContract.CommonDataKinds.Phone.NUMBER, "+$phone")
                .withValue(ContactsContract.CommonDataKinds.Phone.TYPE, ContactsContract.CommonDataKinds.Phone.TYPE_MOBILE).build())
            val email = c.optString("email").takeIf { it.contains('@') }
            if (email != null) ops.add(ContentProviderOperation.newInsert(ContactsContract.Data.CONTENT_URI)
                .withValueBackReference(ContactsContract.Data.RAW_CONTACT_ID, 0)
                .withValue(ContactsContract.Data.MIMETYPE, ContactsContract.CommonDataKinds.Email.CONTENT_ITEM_TYPE)
                .withValue(ContactsContract.CommonDataKinds.Email.ADDRESS, email)
                .withValue(ContactsContract.CommonDataKinds.Email.TYPE, ContactsContract.CommonDataKinds.Email.TYPE_OTHER).build())
            ops.add(ContentProviderOperation.newInsert(ContactsContract.Data.CONTENT_URI)
                .withValueBackReference(ContactsContract.Data.RAW_CONTACT_ID, 0)
                .withValue(ContactsContract.Data.MIMETYPE, ContactsContract.CommonDataKinds.Note.CONTENT_ITEM_TYPE)
                .withValue(ContactsContract.CommonDataKinds.Note.NOTE, note).build())
            // The tag: which CRM row this is, so a re-sync finds it.
            ops.add(ContentProviderOperation.newInsert(ContactsContract.Data.CONTENT_URI)
                .withValueBackReference(ContactsContract.Data.RAW_CONTACT_ID, 0)
                .withValue(ContactsContract.Data.MIMETYPE, MIME_TAG)
                .withValue(ContactsContract.Data.DATA1, id)
                .withValue(ContactsContract.Data.DATA2, businessLabel(ctx)).build())
            runCatching { resolver.applyBatch(ContactsContract.AUTHORITY, ops); added++ }
                .onFailure { android.util.Log.w("OsSync", "contact insert failed", it) }
        }
        if (added > 0) android.util.Log.i("OsSync", "$added contacts added to the phone")
        return added
    }

    private fun Char.isDigitOrPlus() = isDigit() || this == '+'

    private fun findRawContact(ctx: Context, crmId: String): Long? {
        ctx.contentResolver.query(
            ContactsContract.Data.CONTENT_URI, arrayOf(ContactsContract.Data.RAW_CONTACT_ID),
            "${ContactsContract.Data.MIMETYPE} = ? AND ${ContactsContract.Data.DATA1} = ?", arrayOf(MIME_TAG, crmId), null,
        )?.use { if (it.moveToFirst()) return it.getLong(0) }
        return null
    }

    // ------------------------------------------------------------ calendar

    private fun asSyncAdapter(uri: Uri): Uri = uri.buildUpon()
        .appendQueryParameter(CalendarContract.CALLER_IS_SYNCADAPTER, "true")
        .appendQueryParameter(CalendarContract.Calendars.ACCOUNT_NAME, CALENDAR_ACCOUNT)
        .appendQueryParameter(CalendarContract.Calendars.ACCOUNT_TYPE, CalendarContract.ACCOUNT_TYPE_LOCAL).build()

    /** The "agente" local calendar (visible in the phone's calendar app), created on first use. */
    fun ensureCalendar(ctx: Context): Long? {
        val resolver = ctx.contentResolver
        resolver.query(
            CalendarContract.Calendars.CONTENT_URI, arrayOf(CalendarContract.Calendars._ID),
            "${CalendarContract.Calendars.ACCOUNT_TYPE} = ? AND ${CalendarContract.Calendars.NAME} = ?",
            arrayOf(CalendarContract.ACCOUNT_TYPE_LOCAL, CALENDAR_NAME), null,
        )?.use { if (it.moveToFirst()) return it.getLong(0) }
        val v = ContentValues().apply {
            put(CalendarContract.Calendars.ACCOUNT_NAME, CALENDAR_ACCOUNT)
            put(CalendarContract.Calendars.ACCOUNT_TYPE, CalendarContract.ACCOUNT_TYPE_LOCAL)
            put(CalendarContract.Calendars.NAME, CALENDAR_NAME)
            put(CalendarContract.Calendars.CALENDAR_DISPLAY_NAME, ctx.getString(R.string.ossync_calendar_name, businessLabel(ctx)))
            put(CalendarContract.Calendars.CALENDAR_COLOR, ctx.getColor(R.color.agente_primary))
            put(CalendarContract.Calendars.CALENDAR_ACCESS_LEVEL, CalendarContract.Calendars.CAL_ACCESS_OWNER)
            put(CalendarContract.Calendars.OWNER_ACCOUNT, CALENDAR_ACCOUNT)
            put(CalendarContract.Calendars.VISIBLE, 1)
            put(CalendarContract.Calendars.SYNC_EVENTS, 1)
            put(CalendarContract.Calendars.CALENDAR_TIME_ZONE, TimeZone.getDefault().id)
        }
        val uri = resolver.insert(asSyncAdapter(CalendarContract.Calendars.CONTENT_URI), v) ?: return null
        return ContentUris.parseId(uri)
    }

    private val localFmt = SimpleDateFormat("yyyy-MM-dd'T'HH:mm", Locale.US)

    /** Mirrors `/api/appointments` (upcoming, every status) into the agente calendar. Returns (inserted, updated, deleted). */
    fun syncCalendar(ctx: Context, appointments: JSONArray): Triple<Int, Int, Int> {
        val cal = ensureCalendar(ctx) ?: return Triple(0, 0, 0)
        val resolver = ctx.contentResolver
        var ins = 0; var upd = 0; var del = 0
        for (i in 0 until appointments.length()) {
            val a = appointments.getJSONObject(i)
            val id = a.optString("id"); if (id.isBlank()) continue
            val start = runCatching { localFmt.parse(a.optString("startsAt")) }.getOrNull()?.time ?: continue
            val dur = a.optInt("durationMins", 0).takeIf { it > 0 } ?: 45
            val status = a.optString("status")
            val customUri = EVENT_URI_PREFIX + id
            val existing: Long? = resolver.query(
                CalendarContract.Events.CONTENT_URI, arrayOf(CalendarContract.Events._ID),
                "${CalendarContract.Events.CALENDAR_ID} = ? AND ${CalendarContract.Events.CUSTOM_APP_URI} = ?", arrayOf(cal.toString(), customUri), null,
            )?.use { if (it.moveToFirst()) it.getLong(0) else null }
            if (status == "cancelled") {
                if (existing != null) { resolver.delete(ContentUris.withAppendedId(CalendarContract.Events.CONTENT_URI, existing), null, null); del++ }
                continue
            }
            val prefix = when (status) { "done" -> "✓ "; "no_show" -> "✗ "; "pending_payment" -> "⏳ "; else -> "" }
            val customer = a.optString("customer")
            val service = a.optString("service").takeIf { it.isNotBlank() && it != "null" }
            val specialist = a.optString("specialist").takeIf { it.isNotBlank() && it != "null" }
            val v = ContentValues().apply {
                put(CalendarContract.Events.CALENDAR_ID, cal)
                put(CalendarContract.Events.DTSTART, start)
                put(CalendarContract.Events.DTEND, start + dur * 60_000L)
                put(CalendarContract.Events.EVENT_TIMEZONE, TimeZone.getDefault().id)
                put(CalendarContract.Events.TITLE, prefix + listOfNotNull(customer, service).joinToString(" · "))
                put(CalendarContract.Events.DESCRIPTION, listOfNotNull(specialist, a.optString("phone").takeIf { it.isNotBlank() }?.let { "+" + it.substringAfter(':').trimStart('+') }, ctx.getString(R.string.ossync_event_note)).joinToString("\n"))
                put(CalendarContract.Events.CUSTOM_APP_PACKAGE, ctx.packageName)
                put(CalendarContract.Events.CUSTOM_APP_URI, customUri)
                put(CalendarContract.Events.HAS_ALARM, 1)
            }
            if (existing == null) {
                val uri = resolver.insert(CalendarContract.Events.CONTENT_URI, v) ?: continue
                val eventId = ContentUris.parseId(uri)
                resolver.insert(CalendarContract.Reminders.CONTENT_URI, ContentValues().apply {
                    put(CalendarContract.Reminders.EVENT_ID, eventId)
                    put(CalendarContract.Reminders.MINUTES, 60)
                    put(CalendarContract.Reminders.METHOD, CalendarContract.Reminders.METHOD_ALERT)
                })
                ins++
            } else {
                resolver.update(ContentUris.withAppendedId(CalendarContract.Events.CONTENT_URI, existing), v, null, null); upd++
            }
        }
        android.util.Log.i("OsSync", "calendar: +$ins ~$upd -$del")
        return Triple(ins, upd, del)
    }

    // ------------------------------------------------------------- exports

    /** vCard/iCalendar TEXT escaping. A bare CR is dropped: with it, a name
     *  could end the line and start a property of its own. */
    internal fun esc(s: String) = s.replace("\r\n", "\n").replace("\r", "").replace("\\", "\\\\").replace(",", "\\,").replace(";", "\\;").replace("\n", "\\n")

    fun vcard(contacts: JSONArray): String {
        val sb = StringBuilder()
        for (i in 0 until contacts.length()) {
            val c = contacts.getJSONObject(i)
            if (c.optString("kind") == "owner") continue
            val name = c.optString("name").takeIf { it.isNotBlank() && it != "null" } ?: c.optString("phone").takeIf { it.isNotBlank() && it != "null" }?.let { "+$it" } ?: continue
            sb.append("BEGIN:VCARD\r\nVERSION:3.0\r\n")
            sb.append("N:;").append(esc(name)).append(";;;\r\nFN:").append(esc(name)).append("\r\n")
            c.optString("phone").takeIf { it.isNotBlank() && it != "null" }?.let { sb.append("TEL;TYPE=CELL:+").append(it.trimStart('+')).append("\r\n") }
            c.optString("email").takeIf { it.contains('@') }?.let { sb.append("EMAIL:").append(esc(it)).append("\r\n") }
            val notes = listOfNotNull(c.optString("source").takeIf { it.isNotBlank() && it != "null" }, c.optString("notes").takeIf { it.isNotBlank() && it != "null" })
            if (notes.isNotEmpty()) sb.append("NOTE:").append(esc(notes.joinToString(" · "))).append("\r\n")
            sb.append("END:VCARD\r\n")
        }
        return sb.toString()
    }

    fun csv(contacts: JSONArray): String {
        fun q(s: String) = "\"" + s.replace("\"", "\"\"") + "\""
        // A customer picks their own display name: one starting with = + - @
        // (or a tab/CR) would run as a formula when the owner opens the file
        // in a spreadsheet. The apostrophe makes it text (OWASP CSV injection).
        fun text(s: String) = if (s.isNotEmpty() && s[0] in "=+-@\t\r") "'$s" else s
        val sb = StringBuilder("nombre,telefono,email,canal,mensajes,primera_vez,ultima_vez,notas\n")
        for (i in 0 until contacts.length()) {
            val c = contacts.getJSONObject(i)
            if (c.optString("kind") == "owner") continue
            fun f(k: String) = c.optString(k).takeIf { it != "null" } ?: ""
            val phone = f("phone").filter { it.isDigit() }.let { if (it.isBlank()) "" else "+$it" }
            sb.append(listOf(text(f("name")), phone, text(f("email")), text(f("source")), c.optInt("messages").toString(), f("first_seen").ifBlank { f("firstSeen") }, f("last_seen").ifBlank { f("lastSeen") }, text(f("notes"))).joinToString(",") { q(it) }).append('\n')
        }
        return sb.toString()
    }

    fun ics(appointments: JSONArray): String {
        val utc = SimpleDateFormat("yyyyMMdd'T'HHmmss'Z'", Locale.US).apply { timeZone = TimeZone.getTimeZone("UTC") }
        val sb = StringBuilder("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//yaya.tech//agente//ES\r\nCALSCALE:GREGORIAN\r\n")
        for (i in 0 until appointments.length()) {
            val a = appointments.getJSONObject(i)
            if (a.optString("status") == "cancelled") continue
            val start = runCatching { localFmt.parse(a.optString("startsAt")) }.getOrNull() ?: continue
            val dur = a.optInt("durationMins", 0).takeIf { it > 0 } ?: 45
            sb.append("BEGIN:VEVENT\r\nUID:").append(a.optString("id")).append("@agente\r\n")
            sb.append("DTSTAMP:").append(utc.format(java.util.Date())).append("\r\n")
            sb.append("DTSTART:").append(utc.format(start)).append("\r\nDTEND:").append(utc.format(java.util.Date(start.time + dur * 60_000L))).append("\r\n")
            sb.append("SUMMARY:").append(esc(listOfNotNull(a.optString("customer"), a.optString("service").takeIf { it.isNotBlank() && it != "null" }).joinToString(" · "))).append("\r\n")
            sb.append("STATUS:").append(if (a.optString("status") == "pending_payment") "TENTATIVE" else "CONFIRMED").append("\r\nEND:VEVENT\r\n")
        }
        return sb.append("END:VCALENDAR\r\n").toString()
    }

    /** Writes the text to a cache file and opens the share sheet. Call from the main thread. */
    fun shareFile(ctx: Context, fileName: String, mime: String, content: String) {
        val dir = File(ctx.cacheDir, "export").apply { mkdirs() }
        val f = File(dir, fileName).apply { writeText(content) }
        val uri = FileProvider.getUriForFile(ctx, "${ctx.packageName}.fileprovider", f)
        val send = Intent(Intent.ACTION_SEND).setType(mime).putExtra(Intent.EXTRA_STREAM, uri).addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
        ctx.startActivity(Intent.createChooser(send, fileName))
    }
}
