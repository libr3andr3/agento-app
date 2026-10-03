package tech.yaya.agente

import android.content.Context
import org.json.JSONArray
import org.json.JSONObject
import java.util.Locale

/** One business category: a stable key and its label per app language. */
data class Category(val key: String, val es: String, val pt: String, val en: String, val prohibited: Boolean) {
    fun label(ctx: Context): String = when (ctx.resources.configuration.locales[0].language) {
        "pt" -> pt
        "en" -> en
        else -> es
    }
}

/**
 * The fixed category list the registration screen offers, and the
 * prohibited categories the server publishes (docs/CREDITS.md § 4). The
 * server's `GET /api/categories` wins whenever it answered once; the
 * bundled list below is only the fallback for a first launch without
 * network. Prohibited categories are NOT offered in the picker (the menu
 * shows only `prohibited == false`); the gate still applies to any
 * prohibited key that reaches submit (e.g. an older cached list) and
 * lands on the neutral "Agente no está disponible para este tipo de
 * negocio" screen.
 */
object Categories {
    val DEFAULT: List<Category> = listOf(
        Category("peluqueria", "Peluquería / barbería", "Salão / barbearia", "Hair salon / barber", false),
        Category("estetica", "Estética / uñas", "Estética / unhas", "Beauty / nails", false),
        Category("consultorio", "Consultorio / clínica", "Consultório / clínica", "Practice / clinic", false),
        Category("veterinaria", "Veterinaria", "Veterinária", "Veterinary", false),
        Category("gimnasio", "Gimnasio / clases", "Academia / aulas", "Gym / classes", false),
        Category("restaurante", "Restaurante / delivery", "Restaurante / delivery", "Restaurant / delivery", false),
        Category("ropa", "Ropa / tienda", "Roupas / loja", "Clothing / shop", false),
        Category("servicios", "Servicios técnicos", "Serviços técnicos", "Technical services", false),
        Category("otro", "Otro", "Outro", "Other", false),
        Category("farmacia_sin_receta", "Farmacia sin receta", "Farmácia sem receita", "Pharmacy without prescription", true),
        Category("armas", "Armas", "Armas", "Weapons", true),
        Category("apuestas", "Apuestas / casinos", "Apostas / cassinos", "Betting / casinos", true),
        Category("adulto", "Contenido adulto", "Conteúdo adulto", "Adult content", true),
        Category("cripto_intercambio", "Criptointercambio", "Exchange de cripto", "Crypto exchange", true),
        Category("prestamos", "Préstamos", "Empréstimos", "Loans", true),
    )

    /** The current list: what the server last sent, else the bundled default. */
    fun all(ctx: Context): List<Category> {
        val cached = Prefs.categoriesJson(ctx) ?: return DEFAULT
        return runCatching { parse(JSONObject(cached)) }.getOrNull()?.takeIf { it.isNotEmpty() } ?: DEFAULT
    }

    fun byKey(ctx: Context, key: String): Category? = all(ctx).firstOrNull { it.key == key }

    fun isProhibited(ctx: Context, key: String): Boolean = byKey(ctx, key)?.prohibited == true

    /** `{categories:[{key, es, pt, en}], prohibited:[{key, es, pt, en}]}` → the list. */
    fun parse(j: JSONObject): List<Category> {
        fun rows(arr: JSONArray?, prohibited: Boolean): List<Category> = (0 until (arr?.length() ?: 0)).mapNotNull { i ->
            val o = arr!!.optJSONObject(i) ?: return@mapNotNull null
            val key = o.optString("key").lowercase(Locale.US).takeIf { it.isNotBlank() } ?: return@mapNotNull null
            val es = o.optString("es").ifBlank { key }
            Category(key, es, o.optString("pt").ifBlank { es }, o.optString("en").ifBlank { es }, prohibited)
        }
        return rows(j.optJSONArray("categories"), false) + rows(j.optJSONArray("prohibited"), true)
    }

    /** Pulls the server's list if stale. Network: call off the main thread. */
    fun refresh(ctx: Context) {
        if (System.currentTimeMillis() - Prefs.categoriesAt(ctx) < 6 * 3600 * 1000L) return
        val j = ServerClient.categories(ctx) ?: return
        // A payload with only the prohibited list (gateway unreachable from
        // the core) is not a category list: keep whatever we had — but still
        // take the six-hour back-off, or a core stuck on that answer is
        // re-asked at every cold start.
        if (parse(j).any { !it.prohibited }) Prefs.setCategoriesJson(ctx, j.toString()) else Prefs.touchCategoriesAt(ctx)
    }
}
