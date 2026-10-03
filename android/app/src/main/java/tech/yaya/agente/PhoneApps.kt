package tech.yaya.agente

import android.content.Context
import android.content.Intent
import java.text.Collator

/**
 * The apps on this phone, as the read screen ([ReadAppsActivity]) lists
 * them: everything with a launcher icon — visible on the Play build too,
 * through the manifest's launcher `<queries>` intent — plus the chat apps,
 * wallets and learned apps agento already knows, which can post without
 * having an icon of their own.
 */
object PhoneApps {

    /** Why an app is on the list, in the order the list shows them. */
    enum class Kind { CHAT, LEARNED, MONEY, OTHER }

    data class App(val packageName: String, val label: String, val kind: Kind)

    /** Reads the package manager: call off the main thread. */
    fun list(ctx: Context): List<App> {
        val pm = ctx.packageManager
        val labels = HashMap<String, String>()
        val launcher = Intent(Intent.ACTION_MAIN).addCategory(Intent.CATEGORY_LAUNCHER)
        runCatching { pm.queryIntentActivities(launcher, 0) }.getOrDefault(emptyList()).forEach { ri ->
            val info = ri.activityInfo ?: return@forEach
            labels.getOrPut(info.packageName) {
                runCatching { info.applicationInfo.loadLabel(pm).toString() }.getOrDefault(info.packageName)
            }
        }
        val learned = ProfileStore.all(ctx)
        val known = SupportedApps.ALL.map { it.packageName to it.displayName } +
            Wallets.candidates(ctx, Prefs.country(ctx)).map { it.packageName to it.displayName } +
            learned.map { it.packageName to it.displayName }
        known.forEach { (pkg, name) -> if (pkg !in labels && AppToggles.isInstalled(ctx, pkg)) labels[pkg] = name }
        // Chat apps under the same names the reply screen uses.
        SupportedApps.ALL.forEach { if (it.packageName in labels) labels[it.packageName] = it.displayName }
        labels.remove(ctx.packageName)

        val money = Wallets.all(ctx).map { it.packageName }.toSet() + Prefs.learnedPaymentSources(ctx)
        val learnedPkgs = learned.map { it.packageName }.toSet()
        return order(labels.map { (pkg, label) -> App(pkg, label, kindOf(pkg, money, learnedPkgs)) })
    }

    internal fun kindOf(pkg: String, money: Set<String>, learned: Set<String>): Kind = when {
        SupportedApps.isSupported(pkg) -> Kind.CHAT
        pkg in learned -> Kind.LEARNED
        pkg in money -> Kind.MONEY
        else -> Kind.OTHER
    }

    /** Chat apps first, then learned ones, wallets and banks, then the rest — each by name. */
    internal fun order(apps: List<App>): List<App> {
        val byName = Collator.getInstance().apply { strength = Collator.PRIMARY }
        return apps.sortedWith(compareBy<App> { it.kind.ordinal }.thenComparator { a, b -> byName.compare(a.label, b.label) })
    }

    /** The business's own chat app: WhatsApp Business, else WhatsApp. */
    fun businessChat(installed: Collection<String>): String? = when {
        "com.whatsapp.w4b" in installed -> "com.whatsapp.w4b"
        "com.whatsapp" in installed -> "com.whatsapp"
        else -> null
    }

    /**
     * What the read screen suggests for an app the owner never decided on:
     * the chat app the business works from, every wallet and bank, every app
     * this phone learned. Other chat apps and everything else start off —
     * reading a personal inbox, or an app with no reason to be read, is the
     * owner's call to make.
     */
    fun suggested(app: App, installed: Collection<String>): Boolean = when (app.kind) {
        Kind.CHAT -> app.packageName == businessChat(installed)
        Kind.LEARNED, Kind.MONEY -> true
        Kind.OTHER -> false
    }
}
