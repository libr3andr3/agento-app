package tech.yaya.agente

import android.content.Context
import androidx.appcompat.app.AppCompatDelegate
import androidx.core.os.LocaleListCompat

/**
 * The app's language: Spanish (default resources), Portuguese or English,
 * following the phone unless the owner picks one in Ajustes → Idioma.
 * AppCompat applies the choice to every screen and persists it (Android 13+
 * keeps it in system settings; below that, the metadata-holder service in
 * the manifest lets AppCompat store it). Only the app's texts change here —
 * the agent talks to customers in the language of the business's country,
 * which the core decides from the registration locale.
 */
object AppLanguage {
    /** BCP-47 tag ("" = follow the phone) → label. */
    val OPTIONS: List<Pair<String, Int>> = listOf(
        "" to R.string.language_system,
        "es" to R.string.language_es,
        "pt" to R.string.language_pt,
        "en" to R.string.language_en,
    )

    fun currentTag(): String = AppCompatDelegate.getApplicationLocales().toLanguageTags().substringBefore('-')

    fun currentLabel(ctx: Context): String =
        ctx.getString(OPTIONS.firstOrNull { it.first == currentTag() }?.second ?: R.string.language_system)

    fun apply(tag: String) {
        AppCompatDelegate.setApplicationLocales(
            if (tag.isEmpty()) LocaleListCompat.getEmptyLocaleList() else LocaleListCompat.forLanguageTags(tag)
        )
    }
}
