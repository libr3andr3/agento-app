package tech.yaya.agente

import android.content.ContentValues
import android.content.Context
import android.database.sqlite.SQLiteDatabase
import android.database.sqlite.SQLiteOpenHelper
import android.util.Log

/**
 * Every customer message the listener has seen, written down *before* anything
 * is done about it.
 *
 * **Why this exists.** Messaging apps bundle. When five messages arrive from
 * two chats, Instagram and Messenger stop posting one notification per message
 * and post one that says "10 chats from 2 contacts", and the parser gets a
 * single `MessagingStyle` carrying whatever the app felt like including. If the
 * agent only ever answers the newest message and never clears the notification,
 * two things go wrong: earlier messages are never read at all, and the bundle
 * keeps growing until the app collapses it into a summary with no reply action
 * and the agent goes deaf on that chat.
 *
 * So the listener reads *every* message in the notification, writes each one
 * here, and only then replies — and once the reply lands it cancels the
 * notification so the app's bundle resets to empty and the next message arrives
 * as its own clean notification.
 *
 * **Why the unique index is the whole trick.** Apps re-post the same
 * notification constantly (a changing unread count, a delivery receipt), and
 * each re-post replays messages we have already handled. `(channel, handle,
 * sent_at, text_hash)` is stable across those re-posts, so a replayed message
 * hits the conflict clause and is ignored. That single rule is what makes
 * "read the whole bundle every time" safe: re-reading is free, and nothing is
 * answered twice.
 *
 * **What this is not.** Business data lives in the core's `agente.db` and never
 * here (D1 — the phone is the system of record). This is transport bookkeeping:
 * what arrived, what we did about it, and enough to not lose a message if the
 * process dies between reading and replying. Rows are pruned after [KEEP_MS].
 */
object InboxQueue {
    private const val TAG = "InboxQueue"
    private const val DB = "agente_inbox.db"
    private const val VERSION = 1
    private const val TABLE = "inbox"

    /** Long enough to survive a crash loop or an offline stretch, short enough
     *  that the file stays small on a busy phone. */
    private const val KEEP_MS = 7L * 24 * 3600 * 1000

    const val PENDING = "pending"
    const val REPLIED = "replied"
    const val FAILED = "failed"
    const val SKIPPED = "skipped"

    /** One inbound message, normalised. [id] is 0 until it is stored. */
    data class Item(
        val id: Long = 0,
        val channel: String,
        val handle: String,
        val displayName: String,
        val appPackage: String,
        val text: String,
        val sentAt: Long,
        val isGroup: Boolean,
        val notifKey: String?,
    )

    private class Helper(ctx: Context) : SQLiteOpenHelper(ctx, DB, null, VERSION) {
        override fun onCreate(db: SQLiteDatabase) {
            db.execSQL(
                "CREATE TABLE $TABLE (" +
                    "id INTEGER PRIMARY KEY AUTOINCREMENT," +
                    "channel TEXT NOT NULL," +
                    "handle TEXT NOT NULL," +
                    "display_name TEXT NOT NULL DEFAULT ''," +
                    "app_package TEXT NOT NULL," +
                    "text TEXT NOT NULL," +
                    "text_hash INTEGER NOT NULL," +
                    "sent_at INTEGER NOT NULL," +
                    "received_at INTEGER NOT NULL," +
                    "is_group INTEGER NOT NULL DEFAULT 0," +
                    "notif_key TEXT," +
                    "state TEXT NOT NULL," +
                    "reply TEXT," +
                    "attempts INTEGER NOT NULL DEFAULT 0)"
            )
            // The dedupe rule. See the class comment: this is what lets the
            // listener re-read a whole bundle on every repost without answering
            // anything twice.
            db.execSQL(
                "CREATE UNIQUE INDEX idx_inbox_identity " +
                    "ON $TABLE(channel, handle, sent_at, text_hash)"
            )
            db.execSQL("CREATE INDEX idx_inbox_state ON $TABLE(state, received_at)")
        }

        override fun onUpgrade(db: SQLiteDatabase, old: Int, new: Int) {
            // Transport bookkeeping only: nothing here is worth a migration
            // path, and a stale queue replaying old messages would be worse
            // than an empty one.
            db.execSQL("DROP TABLE IF EXISTS $TABLE")
            onCreate(db)
        }
    }

    @Volatile private var helper: Helper? = null

    /** Tests: forget the open database (each test gets a fresh app). */
    internal fun resetForTest() = synchronized(this) { helper?.close(); helper = null }

    private fun db(ctx: Context): SQLiteDatabase? = try {
        synchronized(this) {
            (helper ?: Helper(ctx.applicationContext).also { helper = it }).writableDatabase
        }
    } catch (t: Throwable) {
        // A queue we cannot open must not cost the customer their reply: the
        // caller falls back to answering without persistence.
        Log.e(TAG, "cannot open inbox queue", t)
        null
    }

    /**
     * Stores every message that has not been seen before and returns exactly
     * those, oldest first. Messages already in the queue — the repeats a
     * re-posted notification replays — return nothing, which is the caller's
     * signal that there is nothing new to answer.
     */
    fun addNew(ctx: Context, items: List<Item>): List<Item> {
        val db = db(ctx) ?: return items // no queue: treat everything as new
        val fresh = ArrayList<Item>(items.size)
        val now = System.currentTimeMillis()
        for (it in items) {
            val cv = ContentValues().apply {
                put("channel", it.channel)
                put("handle", it.handle)
                put("display_name", it.displayName)
                put("app_package", it.appPackage)
                put("text", it.text)
                put("text_hash", it.text.hashCode())
                put("sent_at", it.sentAt)
                put("received_at", now)
                put("is_group", if (it.isGroup) 1 else 0)
                put("notif_key", it.notifKey)
                put("state", PENDING)
            }
            val id = try {
                db.insertWithOnConflict(TABLE, null, cv, SQLiteDatabase.CONFLICT_IGNORE)
            } catch (t: Throwable) {
                Log.w(TAG, "insert failed", t)
                -1L
            }
            if (id > 0) fresh.add(it.copy(id = id))
        }
        if (fresh.isNotEmpty()) prune(db, now)
        return fresh
    }

    /** Marks what happened to a message. [reply] is kept for the owner's log. */
    fun settle(ctx: Context, ids: List<Long>, state: String, reply: String? = null) {
        if (ids.isEmpty()) return
        val db = db(ctx) ?: return
        val cv = ContentValues().apply {
            put("state", state)
            if (reply != null) put("reply", reply)
        }
        val where = "id IN (${ids.joinToString(",") { "?" }})"
        try {
            db.update(TABLE, cv, where, ids.map { it.toString() }.toTypedArray())
        } catch (t: Throwable) {
            Log.w(TAG, "settle failed", t)
        }
    }

    /**
     * This customer's messages that were written down but never answered — the
     * ones a crash, an offline stretch or an unreachable core stranded.
     *
     * They are not retried on a timer, because by the time we could retry, the
     * notification carrying the reply action is usually gone and there is
     * nothing to answer *through*. Instead they ride along on this customer's
     * next message: the agent is told what they said while we were deaf, in
     * order, and answers all of it at once. A customer who wrote "¿tienen
     * delivery?" during an outage and "¿hola?" after it gets one reply to both,
     * not a reply that ignores the first question.
     *
     * Oldest first; [excluding] drops the rows the caller has just inserted.
     */
    fun pendingFor(
        ctx: Context,
        channel: String,
        handle: String,
        excluding: List<Long> = emptyList(),
        limit: Int = 20,
    ): List<Item> {
        val db = db(ctx) ?: return emptyList()
        return try {
            db.query(
                TABLE, null, "state = ? AND channel = ? AND handle = ?",
                arrayOf(PENDING, channel, handle),
                null, null, "sent_at ASC", limit.toString()
            ).use { c ->
                val out = ArrayList<Item>()
                while (c.moveToNext()) {
                    out.add(
                        Item(
                            id = c.getLong(c.getColumnIndexOrThrow("id")),
                            channel = c.getString(c.getColumnIndexOrThrow("channel")),
                            handle = c.getString(c.getColumnIndexOrThrow("handle")),
                            displayName = c.getString(c.getColumnIndexOrThrow("display_name")),
                            appPackage = c.getString(c.getColumnIndexOrThrow("app_package")),
                            text = c.getString(c.getColumnIndexOrThrow("text")),
                            sentAt = c.getLong(c.getColumnIndexOrThrow("sent_at")),
                            isGroup = c.getInt(c.getColumnIndexOrThrow("is_group")) != 0,
                            notifKey = c.getString(c.getColumnIndexOrThrow("notif_key")),
                        )
                    )
                }
                out.filterNot { it.id in excluding }
            }
        } catch (t: Throwable) {
            Log.w(TAG, "pending query failed", t)
            emptyList()
        }
    }

    private fun prune(db: SQLiteDatabase, now: Long) {
        try {
            db.delete(TABLE, "received_at < ?", arrayOf((now - KEEP_MS).toString()))
        } catch (t: Throwable) {
            Log.w(TAG, "prune failed", t)
        }
    }
}
