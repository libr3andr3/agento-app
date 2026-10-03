package tech.yaya.agente

import android.content.Context
import org.json.JSONObject
import java.util.Locale

/**
 * A money app the agent may read payment notices from: package name, the
 * name people use for it, and the countries where it is common.
 */
data class Wallet(val packageName: String, val displayName: String, val countries: Set<String>)

/**
 * Money apps by country — a *hint list*, never a gate.
 *
 * Payment detection itself is universal ([PaymentDetector] forwards any
 * non-chat notification to the local agent, which decides whether it carries
 * money), so a wallet missing here still works. What this catalog adds is
 * the end-of-onboarding screen: the wallets and banks people in the owner's
 * country actually use, pre-checked when installed on this phone, so the
 * owner sees at a glance what the agent will read and can switch any of
 * them off. The country comes from the WhatsApp number the owner registered
 * with ([Prefs.country]); a migrant owner with a wallet from home still sees
 * it, because installed wallets from any country are listed too.
 *
 * [ALL] is the bundled default; the server pushes the live catalog as JSON
 * (`GET /api/wallets`, [refresh]) so a wrong package is fixed without an
 * app release. The long tail is best-effort and self-correcting — a wrong
 * package simply never shows as installed, and the packages the network
 * learned (`Prefs.learnedPaymentSources`) are merged on top at runtime.
 * Data stays on the phone: nothing in this file is ever sent anywhere.
 */
object Wallets {

    private val PE = setOf("PE"); private val MX = setOf("MX"); private val CO = setOf("CO")
    private val AR = setOf("AR"); private val CL = setOf("CL"); private val BR = setOf("BR")
    private val EC = setOf("EC"); private val BO = setOf("BO"); private val UY = setOf("UY")
    private val PY = setOf("PY"); private val VE = setOf("VE"); private val GT = setOf("GT")
    private val CR = setOf("CR"); private val PA = setOf("PA"); private val DO = setOf("DO")
    private val SV = setOf("SV"); private val HN = setOf("HN"); private val NI = setOf("NI")
    private val US = setOf("US"); private val ES = setOf("ES"); private val PT = setOf("PT")
    private val LATAM = setOf("AR", "BO", "BR", "CL", "CO", "CR", "DO", "EC", "GT", "HN", "MX", "NI", "PA", "PE", "PY", "SV", "UY", "VE")

    val ALL: List<Wallet> = listOf(
        // Perú
        Wallet("com.bcp.innovacxion.yapeapp", "Yape", PE),
        Wallet("com.bcp.bank.bcp", "BCP", PE),
        Wallet("pe.com.interbank.mobilebanking", "Interbank (Plin)", PE),
        Wallet("com.bbva.nxt_peru", "BBVA Perú (Plin)", PE),
        Wallet("pe.com.scotiabank.blpm.android.client", "Scotiabank Perú (Plin)", PE),
        Wallet("pe.com.bn.bnmovil", "Banco de la Nación", PE),
        // México
        Wallet("com.bancomer.mbanking", "BBVA México", MX),
        Wallet("com.banorte.movil", "Banorte Móvil", MX),
        Wallet("mx.com.santander.supermovil", "Santander México", MX),
        Wallet("com.banamex.mobile", "Citibanamex", MX),
        Wallet("com.oxxo.spin", "Spin by OXXO", MX),
        // Colombia
        Wallet("com.nequi.MobileApp", "Nequi", CO),
        Wallet("com.davivienda.daviplataapp", "Daviplata", CO),
        Wallet("com.todo1.mobile", "Bancolombia", CO),
        Wallet("com.bbva.bbvacolombia", "BBVA Colombia", CO),
        Wallet("co.movii", "Movii", CO),
        // Argentina
        Wallet("ar.com.uala", "Ualá", AR),
        Wallet("ar.com.modo", "MODO", AR),
        Wallet("com.brubank", "Brubank", AR),
        Wallet("ar.com.bancoprovincia.cuentadni", "Cuenta DNI", AR),
        Wallet("com.naranja.nx", "Naranja X", AR),
        Wallet("ar.com.personalpay", "Personal Pay", AR),
        // Chile
        Wallet("cl.bci.mach", "MACH", CL),
        Wallet("cl.tenpo.app", "Tenpo", CL),
        Wallet("cl.bancoestado.app", "BancoEstado", CL),
        Wallet("cl.bancochile.mobile", "Banco de Chile", CL),
        Wallet("cl.santander.smartphone", "Santander Chile", CL),
        // Brasil (Pix lives inside every bank app)
        Wallet("com.nu.production", "Nubank", BR + MX + CO),
        Wallet("com.picpay", "PicPay", BR),
        Wallet("br.com.bb.android", "Banco do Brasil", BR),
        Wallet("com.itau", "Itaú", BR),
        Wallet("com.bradesco", "Bradesco", BR),
        Wallet("br.gov.caixa.tem", "Caixa Tem", BR),
        Wallet("br.com.intermedium", "Inter", BR),
        Wallet("com.c6bank.app", "C6 Bank", BR),
        Wallet("com.santander.app", "Santander Brasil", BR),
        Wallet("br.com.uol.ps.myaccount", "PagBank", BR),
        // Ecuador
        Wallet("com.pichincha.deuna", "Deuna", EC),
        Wallet("com.bancopichincha.bancamovil", "Banco Pichincha", EC),
        Wallet("com.bancoguayaquil.app", "Banco Guayaquil", EC),
        Wallet("com.produbanco.app", "Produbanco", EC),
        // Bolivia
        Wallet("bo.com.bnb.app", "BNB", BO),
        Wallet("bo.com.bancounion.app", "Banco Unión", BO),
        Wallet("com.tigo.money.bo", "Tigo Money", BO),
        // Uruguay
        Wallet("uy.com.prex", "Prex", UY),
        Wallet("uy.com.brou.app", "BROU", UY),
        Wallet("uy.com.oca.app", "OCA", UY),
        // Paraguay
        Wallet("py.com.tigo.money", "Tigo Money", PY),
        Wallet("py.com.personal.pay", "Personal Pay", PY),
        Wallet("py.com.ueno.app", "ueno", PY),
        Wallet("py.com.zimple", "Zimple", PY),
        // Venezuela (Pago Móvil lives inside the bank apps)
        Wallet("com.banesco.banescomovil", "Banesco", VE),
        Wallet("com.mercantil.mercantilmovil", "Mercantil", VE),
        Wallet("com.bdv.bdvapp", "Banco de Venezuela", VE),
        Wallet("com.bbva.provinet", "BBVA Provincial", VE),
        // Centroamérica y Caribe
        Wallet("com.bi.bienlinea", "Bi en Línea", GT),
        Wallet("com.banrural.app", "Banrural", GT),
        Wallet("com.bac.credomatic", "BAC Credomatic", GT + CR + PA + SV + HN + NI),
        Wallet("cr.bncr.app", "BN Móvil (SINPE)", CR),
        Wallet("cr.bancobcr.app", "BCR Móvil", CR),
        Wallet("com.bgeneral.yappy", "Yappy", PA),
        Wallet("com.bgeneral.app", "Banco General", PA),
        Wallet("com.bpd.app", "Banco Popular", DO),
        Wallet("com.banreservas.app", "Banreservas", DO),
        Wallet("com.bancoagricola.app", "Banco Agrícola", SV),
        Wallet("com.tigo.money.hn", "Tigo Money", HN),
        Wallet("com.banpro.app", "Banpro", NI),
        // Regional
        Wallet("com.mercadopago.wallet", "Mercado Pago", LATAM),
        Wallet("com.paypal.android.p2pmobile", "PayPal", LATAM + US + ES + PT),
        // Estados Unidos
        Wallet("com.venmo", "Venmo", US),
        Wallet("com.squareup.cash", "Cash App", US),
        Wallet("com.zellepay.zelle", "Zelle", US),
        Wallet("com.chase.sig.android", "Chase", US),
        Wallet("com.infonow.bofa", "Bank of America", US),
        Wallet("com.wf.wellsfargomobile", "Wells Fargo", US),
        // España y Portugal
        Wallet("es.lacaixa.mobile.android.newwapicon", "CaixaBank (Bizum)", ES),
        Wallet("com.bbva.bbvacontigo", "BBVA España (Bizum)", ES),
        Wallet("es.bancosantander.apps", "Santander España (Bizum)", ES),
        Wallet("com.revolut.revolut", "Revolut", ES + PT + US),
        Wallet("pt.sibs.android.mbway", "MB WAY", PT),
    )

    /**
     * The live catalog: what the server last pushed (`GET /api/wallets`,
     * cached in [Prefs.walletsJson]), else [ALL]. A server list replaces the
     * bundled one wholesale, so a wrong package can be corrected without an
     * app release. Parsed on every call — the list is small and the callers
     * are screens and one notification path.
     */
    fun all(ctx: Context): List<Wallet> {
        val cached = Prefs.walletsJson(ctx) ?: return ALL
        return runCatching { parse(JSONObject(cached)) }.getOrNull()?.takeIf { it.isNotEmpty() } ?: ALL
    }

    /** `{version, wallets:[{package, name, countries:[…]}]}` → the list. */
    fun parse(j: JSONObject): List<Wallet> {
        val arr = j.optJSONArray("wallets") ?: return emptyList()
        return (0 until arr.length()).mapNotNull { i ->
            val o = arr.optJSONObject(i) ?: return@mapNotNull null
            val pkg = o.optString("package").takeIf { it.isNotBlank() } ?: return@mapNotNull null
            val countries = o.optJSONArray("countries")?.let { c -> (0 until c.length()).map { c.optString(it).uppercase(Locale.US) } }?.toSet() ?: emptySet()
            Wallet(pkg, o.optString("name").ifBlank { pkg }, countries)
        }
    }

    /** Pulls the server's catalog if stale. Network: call off the main thread. */
    fun refresh(ctx: Context) {
        if (System.currentTimeMillis() - Prefs.walletsAt(ctx) < 24 * 3600 * 1000L) return
        val j = ServerClient.wallets(ctx) ?: return
        if (parse(j).isNotEmpty()) Prefs.setWalletsJson(ctx, j.toString())
    }

    fun get(ctx: Context, packageName: String): Wallet? = all(ctx).firstOrNull { it.packageName == packageName }
    fun isKnown(ctx: Context, packageName: String): Boolean = get(ctx, packageName) != null

    /**
     * What the end-of-onboarding screen shows: the country's wallets, then
     * any other catalog wallet that happens to be installed (a wallet from
     * home for a migrant owner), then packages the network reported as money
     * apps in this country ([Prefs.learnedPaymentSources]) that are not in
     * the catalog. Uninstalled wallets from other countries are left out —
     * a switch for an app that cannot post a notification is a lie.
     */
    fun candidates(ctx: Context, iso: String): List<Wallet> {
        val pm = ctx.packageManager
        fun installed(pkg: String) = runCatching { pm.getPackageInfo(pkg, 0) }.isSuccess
        val catalog = all(ctx)
        val local = catalog.filter { iso in it.countries }
        val elsewhere = catalog.filter { iso !in it.countries && installed(it.packageName) }
        val learned = Prefs.learnedPaymentSources(ctx)
            .filter { pkg -> catalog.none { it.packageName == pkg } && installed(pkg) }
            .map { pkg ->
                val label = runCatching { pm.getApplicationLabel(pm.getApplicationInfo(pkg, 0)).toString() }.getOrDefault(pkg)
                Wallet(pkg, label, setOf(iso))
            }
        return (local + elsewhere + learned).distinctBy { it.packageName }
    }
}
