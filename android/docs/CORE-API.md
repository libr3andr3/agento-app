# La API loopback del núcleo

El caparazón Kotlin le habla al núcleo del agente por HTTP plano en
`http://127.0.0.1:<puerto>`; `AgenteCore.baseUrl()` da la URL base y arranca
el núcleo si hace falta. Todas las llamadas pasan por `ServerClient.kt` —
agrega endpoints nuevos ahí, nunca abras un socket en otra parte.

## Cabeceras

| cabecera | cuándo | qué |
|---|---|---|
| `X-App-Key: <secreto>` | cada petición | secreto aleatorio por instalación (`AgenteCore.appKey`); otra app del teléfono no puede manejar el núcleo |
| `Authorization: Bearer <device token>` | rutas de negocio | lo emite `/api/onboard_business`, se guarda cifrado (`Prefs.deviceToken`) |

Los cuerpos son JSON salvo que se indique. Un 2xx trae la respuesta; la app
mapea los demás códigos con `ServerClient.classify()`:

| código | qué significa para el usuario |
|---|---|
| 0 | nunca llegó al núcleo (está caído o arrancando) |
| 401 / 403 | token rechazado — re-emparejar, no reintentar |
| 429 | limitado (topes diarios, intentos de código) |
| 503 | función apagada a propósito (p. ej. verificación de teléfono no desplegada) |
| otros 5xx | problema del núcleo, no del usuario |
| otros 4xx | entrada inválida (foto ilegible, código incorrecto) |

## Endpoints que usa la app

### Cuenta (Yaya ID) — sin bearer

| método + ruta | cuerpo | notas |
|---|---|---|
| `GET /api/account` | | quién tiene sesión, `shareTraining` |
| `POST /api/account/otp/start` | `{email, phone, name?}` | envía un código de 6 dígitos por WhatsApp (el correo es opcional) |
| `POST /api/account/otp/check` | `{email, phone, code, name?}` | crea la cuenta en el primer inicio (con sus créditos de bienvenida); vincula este teléfono. **No hay modo invitado** |
| `POST /api/account/share` | `{share}` | opt-in/out de compartir turnos redactados |
| `POST /api/account/logout` | `{}` | |
| `GET /api/credits` | | saldo, estado (`ok`/`low`/`grace`/`manual`), precios, mes, `depositsEnabled`, `termsVersion`, recarga y ledger (bearer cuando existe un negocio) — `docs/CREDITS.md` |
| `POST /api/topup/session` | `{amount, method}` | `{url}` de tarjeta (Dodo) o la referencia Yape/Plin |
| `GET /api/wallets` · `GET /api/categories` | | catálogos que la pasarela empuja (billeteras por país; categorías y lista prohibida) |
| `POST /api/backup` | `{}` | snapshot cifrado a la cuenta (toda cuenta con sesión) |
| `POST /api/restore` | `{force}` | trae el último snapshot de la cuenta a este teléfono (lento) |

### Registro — sin bearer

| método + ruta | cuerpo | notas |
|---|---|---|
| `POST /api/verify/start` | `{phone}` | 200 código enviado · 503 verificación apagada · 429 · 400. **Un 503 ya no salta el paso**: la app solo sigue sin token si el número es el de la cuenta Yaya (ya verificado por OTP); cualquier otro número se detiene ahí (`reg_verify_unavailable_error`) |
| `POST /api/verify/check` | `{phone, code}` | → `{verificationToken}` |
| `POST /api/onboard_business` | `{businessName, industry, ownerPhone, country, verificationToken?, category, termsVersion, termsAcceptedAt, network?}` | crea el negocio, devuelve el **device token** y el locale; `network: false` solo viaja cuando el dueño apagó el interruptor de la red (ausente = `networkPublish: true`); `403 {"error":"prohibited_category"}` |

### El negocio — bearer

| método + ruta | cuerpo | notas |
|---|---|---|
| `POST /api/execute_action` | `{phoneNumber, message}` | **entra un mensaje de cliente, sale la respuesta del agente**: `{agentResponse, action?, actionData?, attention?[]}`. `action == "no_credits"` = modo manual: la cuenta pasó el piso de gracia, el agente no respondió (`agentResponse` vacío) y el mensaje va al dueño en `attention` (`docs/CREDITS.md` § 2; `limit_reached` en núcleos viejos); `attention[]` = `{question, urgent, gapId}` para el dueño |
| `POST /api/onboarding_message` | `{message}` | un turno de la entrevista de configuración / chat del gerente |
| `POST /api/voice_message` | bytes crudos `audio/m4a` (`application/octet-stream`) | → transcripción + respuesta + `wav` |
| `POST /api/catalog_photo` | bytes crudos JPEG | → ítems y precios; 422 ilegible, 503 apagado |
| `GET /api/dashboard` | | hoy/semana/mes, agenda, conversaciones, preguntas abiertas, `credits` (resumen del saldo) |
| `POST /api/answer_gap` | `{gapId, answer}` | el dueño responde una pregunta que el agente no pudo; se monta al instante |
| `POST /api/payment_event` | el sobre crudo de la notificación (`PaymentDetector`) | → `{handled, matchedAppointment?, matchedOrder?, mute?, untilMs?}` |
| `GET /api/payment_sources` | | paquetes que la red conoce como apps de dinero en este país |
| `GET /api/conversations` · `GET /api/conversations/{peer}` | | vistas del CRM |
| `GET /api/contacts?q=` · `POST /api/contacts/{id}` | patch | |
| `GET /api/payout` · `POST /api/payout` | `{holder, cashOnly, destinations[]}` | Cobros — a dónde envían el dinero los clientes |
| `GET /api/rails?country=` | | nombres de billeteras que la gente usa en este país (sin bearer) |
| `POST /api/location` | `{…}` | ubicación aproximada → la ubicación pública del negocio |

### La app del dueño (D15) — bearer

| método + ruta | cuerpo | notas |
|---|---|---|
| `GET /api/ui` | | `{ui, uiDesigned, businessKind}` — el spec de UI que el núcleo compuso: la plantilla de la vertical ⊕ lo que el agente diseñó con `design_ui`. También dentro de `/api/dashboard` como `ui`, con `businessHours`, `slotDuration`, `products`, `media` para los bloques |
| `POST /api/orders/{id}` | `{status}` | `done` (swipe), `undo`, `cancelled`, `paid` (el dueño cobró en efectivo) |
| `POST /api/appointments/{id}` | `{status}` | `done`, `undo`, `cancelled`, `no_show`, `paid` |
| `POST /api/outcomes/{id}/reverse` | | el dueño cancela/reembolsa un resultado cobrado hace menos de 24 h: el crédito vuelve |
| `GET /api/media` | | las fotos del catálogo sin bytes: `{media: [{id, product, caption, mime, size, createdAt}]}` |
| `POST /api/media?product=&caption=` | bytes crudos JPEG (`image/jpeg`, ≤ 6 MB) | → `{id}` |
| `GET /api/media/{id}` | | los bytes |
| `POST /api/media/{id}` · `POST /api/media/{id}/delete` | `{product?, caption?}` | renombrar / eliminar |
| `POST /api/media/share` | `{ids?[], products?[], note?}` | acuña un **enlace privado** vía la pasarela (`POST /v1/drop`, 5 minutos): `{url, expiresAt, photos}`; 404 nada que compartir, 503 pasarela inalcanzable |

El spec de UI: `{version, home, tabs: [{id, label, icon, intro?, blocks: [{type, opts?}]}]}`.
Los tipos de bloque son el catálogo de la app (`Blocks.kt` ↔ `ui.rs::BLOCKS`):
`earnings`, `attention`, `orders_board`, `agenda_day`, `agenda_week`,
`catalog`, `conversations`, `contacts`. Iconos: `home orders agenda catalog
chats people money star`.

### Cadena de auditoría — bearer

| método + ruta | notas |
|---|---|
| `GET /api/audit?before=&limit=` | entradas de la más nueva a la más vieja: `{seq, ts, kind, actor, subject, payload, hash, anchored}` |
| `GET /api/audit/verify` | recorre la cadena completa: hashes recalculados, enlaces, firmas Ed25519 contra la identidad de esta instalación, tiempo monotónico, la última ancla de la pasarela → `{ok, entries, head, problems[], anchor, unanchoredEntries}` |
| `POST /api/audit/anchor` | pide a la pasarela contrafirmar la cabeza ahora (`POST /v1/audit/anchor`); también ocurre solo cada 25 entradas / 15 min |

Tipos: `customer_turn`, `owner_turn`, `tool_call`, `owner_cmd` (consola web
por el relay), `payment`, `status` (swipes), `share`, `restore`, `boot`. La
tabla es append-only por trigger de SQLite, nunca se exporta en respaldos, y
cada fila es
`hash = sha256("agente-audit-v1\n{seq}\n{ts}\n{kind}\n{actor}\n{subject}\n{payload}\n{prev_hash}")`,
`sig = agent.sign(hash)`.

### Identidad (la usa `DeviceAttestation`) — sin bearer

| método + ruta | notas |
|---|---|
| `GET /api/agent` | la tarjeta pública del agente, incl. el id `agent:<hex>` |
| `POST /api/agent/device` | `{chain[], level}` — cadena de atestación de clave de Android para la tarjeta |

El peer es la clave de la conversación en el núcleo, y por lo tanto una
frontera de autorización: de ella cuelga todo lo que el agente recuerda de
un cliente. Hoy es `"<paquete>:<nombre del remitente>"`
(`SenderRef.legacyPeer`), y la persona viaja aparte como `(channel, handle)`
para que el núcleo junte a un mismo cliente que escribe desde varias apps.

**Límite conocido.** Cuando la app de chat no da un número, el handle es el
nombre para mostrar, y ese nombre lo elige quien escribe: un cliente que se
renombra igual que otro entra en la conversación de ese otro. La corrección
(la identidad que publica la propia app — `Person.key`, o `shortcutId` en un
grupo — y `name:` para los peers que solo tienen nombre) está escrita en la
rama `laptop/agento-1.24.0-security-wip`. Falta comprobar en teléfonos reales
que esas claves son estables por app, y migrar `messages.peer` y
`person_links.handle` en el núcleo: cambiarlos en caliente dejaría cada hilo
en blanco.

## Timeouts y reintentos

Los endpoints de conversación tienen un timeout de lectura de 180 s (un bucle
de herramientas del LLM puede ser lento); los GET tipo panel, 15–30 s. Solo
los GET idempotentes se auto-reintentan (`Retry.IDEMPOTENT`: una vez, tras
500 ms, en código 0 o 5xx). `verify/*` reintenta solo en código 0
(`NETWORK_FAILURE_ONLY`) — un código incorrecto no debe re-probarse. Todo lo
que tiene efecto secundario es un solo disparo.

## Superficie JNI (`AgenteCore.kt`)

```kotlin
external fun start(configJson: String): Int   // arranca el núcleo, devuelve el puerto loopback (≤0 = falló)
external fun stop()
external fun port(): Int                       // 0 cuando no está corriendo
external fun version(): String                 // p. ej. "0.2.0"
```

Claves de `configJson`: `DATABASE_URL` (ruta sqlite), `SCHEMAS_DIR`,
`BIND_ADDR` (`127.0.0.1:0`), `APP_KEY`, `ADMIN_KEY`, `IDENTITY_KEK_HEX`
(32 bytes hex, envuelto por el Keystore en reposo), `DEVICE_LABEL`, y
opcionalmente `LLM_BASE_URL`, `LLM_API_KEY`, `LLM_MODEL` para un endpoint de
modelo propio.
