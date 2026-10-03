package tech.yaya.agente

import org.json.JSONArray
import org.json.JSONObject

/**
 * The owner's app as data (DECISIONS D15). The core composes it — the
 * vertical's template ⊕ what the agent designed with `design_ui` — and the
 * dashboard draws it from the block catalog in [Blocks]. Parsing is lenient
 * and mirrors the core's validation: an unknown block is skipped, an empty
 * tab is dropped, and when nothing is left [fallback] gives the same
 * kind-derived default the core would.
 */
class UiSpec(val home: String, val tabs: List<Tab>) {

    class Tab(val id: String, val label: String, val icon: String, val intro: String?, val blocks: List<Block>)

    class Block(val type: String, val opts: JSONObject?)

    fun homeIndex(): Int = tabs.indexOfFirst { it.id == home }.coerceAtLeast(0)

    /** A stable fingerprint so the bottom bar is rebuilt only when the design changed. */
    fun signature(): String = tabs.joinToString("|") { t -> t.id + ":" + t.label + ":" + t.icon + ":" + t.blocks.joinToString(",") { it.type } }

    companion object {
        val BLOCKS = setOf("earnings", "attention", "orders_board", "agenda_day", "agenda_week", "catalog", "conversations", "contacts")

        fun parse(json: JSONObject?): UiSpec? {
            val arr = json?.optJSONArray("tabs") ?: return null
            val tabs = ArrayList<Tab>()
            for (i in 0 until arr.length()) {
                val t = arr.optJSONObject(i) ?: continue
                val label = t.optString("label").trim()
                if (label.isEmpty()) continue
                val blocks = ArrayList<Block>()
                val bs = t.optJSONArray("blocks") ?: JSONArray()
                for (j in 0 until bs.length()) {
                    val b = bs.opt(j)
                    val type: String; val opts: JSONObject?
                    when (b) {
                        is String -> { type = b.trim().lowercase(); opts = null }
                        is JSONObject -> { type = b.optString("type").trim().lowercase(); opts = b.optJSONObject("opts") }
                        else -> continue
                    }
                    if (type in BLOCKS && blocks.none { it.type == type }) blocks.add(Block(type, opts))
                }
                if (blocks.isEmpty()) continue
                val id = t.optString("id").trim().ifEmpty { "tab$i" }
                val icon = t.optString("icon").trim().lowercase().ifEmpty { iconFor(blocks) }
                val intro = t.optString("intro").trim().takeIf { it.isNotEmpty() && it != "null" }
                tabs.add(Tab(id, label, icon, intro, blocks))
                if (tabs.size == 4) break
            }
            if (tabs.isEmpty()) return null
            val home = json.optString("home").trim().takeIf { h -> tabs.any { it.id == h } } ?: tabs[0].id
            return UiSpec(home, tabs)
        }

        /** Same shape as the core's `default_template`, for a cache that predates D15. */
        fun fallback(ctx: android.content.Context, businessKind: String?): UiSpec {
            val s = { r: Int -> ctx.getString(r) }
            fun b(vararg t: String) = t.map { Block(it, null) }
            val tabs = when (businessKind) {
                "services" -> listOf(
                    Tab("agenda", s(R.string.tab_agenda), "agenda", null, b("earnings", "attention", "agenda_day", "agenda_week")),
                    Tab("clientes", s(R.string.tab_customers), "chats", null, b("conversations", "contacts")),
                )
                "products" -> listOf(
                    Tab("pedidos", s(R.string.tab_orders), "orders", null, b("earnings", "attention", "orders_board")),
                    Tab("catalogo", s(R.string.tab_catalog), "catalog", null, b("catalog")),
                    Tab("clientes", s(R.string.tab_customers), "chats", null, b("conversations", "contacts")),
                )
                else -> listOf(
                    Tab("hoy", s(R.string.tab_today), "home", null, b("earnings", "attention", "orders_board", "agenda_day")),
                    Tab("agenda", s(R.string.tab_agenda), "agenda", null, b("agenda_week")),
                    Tab("catalogo", s(R.string.tab_catalog), "catalog", null, b("catalog")),
                    Tab("clientes", s(R.string.tab_customers), "chats", null, b("conversations", "contacts")),
                )
            }
            return UiSpec(tabs[0].id, tabs)
        }

        private fun iconFor(blocks: List<Block>): String = when (blocks.first().type) {
            "orders_board" -> "orders"
            "agenda_day", "agenda_week" -> "agenda"
            "catalog" -> "catalog"
            "conversations", "contacts" -> "chats"
            "earnings" -> "money"
            else -> "home"
        }

        fun iconRes(name: String): Int = when (name) {
            "orders" -> R.drawable.ic_tab_orders
            "agenda" -> R.drawable.ic_tab_agenda
            "catalog" -> R.drawable.ic_tab_catalog
            "chats" -> R.drawable.ic_tab_chats
            "people" -> R.drawable.ic_tab_people
            "money" -> R.drawable.ic_tab_money
            "star" -> R.drawable.ic_tab_star
            else -> R.drawable.ic_tab_home
        }
    }
}
