package tech.yaya.agente

import org.json.JSONObject

/**
 * The pure half of "Sitio web e integración": what the owner typed, as the
 * core's /api/ops wants it. The core validates for real (domain, https,
 * placeholders); this only tidies and previews.
 */
object WebsiteForm {
    /** "https://www.Example.com/x" → "www.example.com". */
    fun domainOf(input: String): String =
        input.trim().lowercase()
            .removePrefix("https://").removePrefix("http://")
            .substringBefore('/').substringBefore('?').substringBefore('#')
            .substringBefore(':')

    fun defaultTemplate(domain: String): String = if (domain.isEmpty()) "" else "https://$domain/perfil/{id}"

    /** The same sample participant the core's `ops::example` uses. */
    fun preview(template: String): String = template
        .replace("{id}", "p7k2m9x4qa")
        .replace("{role}", "productor")
        .replace("{phone}", "51987654321")
        .replace("{slug}", "rosa-quispe")

    /** An empty profile URL means "the default on my domain". */
    fun body(domain: String, profileUrl: String, webhookUrl: String, sendProfileLink: Boolean): JSONObject =
        JSONObject()
            .put("domain", domainOf(domain))
            .put("profileUrl", profileUrl.trim())
            .put("webhookUrl", webhookUrl.trim())
            .put("sendProfileLink", sendProfileLink)
}
