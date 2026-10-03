# Créditos prepagados: el modelo de ingresos

Desde 1.22.0 agente no vende planes. Cada cuenta Yaya tiene un **saldo de
créditos prepagados en dólares** (circuito cerrado) que se consume solo
cuando se **confirma un resultado**: una cita o una venta. Este documento es
la fuente de verdad del modelo; la app (este repo), el núcleo y la pasarela
(repo `agente.ceo`: `server/src/outcomes.rs`, `gateway/src/prepaid.rs`) lo
implementan tal cual. Un ingeniero nuevo debería poder reproducirlo desde
aquí.

## 1. Precios

| concepto | valor |
|---|---|
| resultado confirmado con un cliente **conocido** | **USD 1.00** |
| resultado confirmado con un cliente **nuevo** | **USD 2.00** |
| volumen: a partir del resultado cobrado nº 101 en el mes calendario | precios a la mitad: USD 0.50 / 1.00 |
| tope duro mensual | **USD 199.00** por cuenta y mes calendario |
| regalo de bienvenida | **USD 12.00** al crear la cuenta (= 6 clientes nuevos), bucket *free* |
| reversión | cancelación o reembolso dentro de las **24 h** del cargo → crédito devuelto a los lotes de donde salió |

- Precios en USD, **impuestos incluidos**. El impuesto solo existe sobre
  dinero pagado (§ 6).
- **Cliente nuevo** = primera vez que ese número de teléfono tiene un
  resultado confirmado con **este negocio**. Por negocio, no global.
- **Confirmado** =
  - con **adelantos habilitados** para el país del negocio
    (`depositsEnabled = true`): el turno quedó reservado **y** llegó la
    notificación del adelanto (o, si el negocio no configuró adelanto, el
    turno quedó reservado);
  - con adelantos **deshabilitados**: el dueño aceptó — el turno quedó
    reservado / el pedido quedó aceptado (`status = confirmed`).
  - Nunca se cobra por chats, cotizaciones, ni pedidos abandonados
    (`pending_payment` que nunca se pagó).
- Volumen y tope se calculan **en la pasarela** sobre el monto cobrado,
  sin importar de qué bucket salió. El mes calendario se cierra en
  `EXPIRY_TZ` (America/Lima por defecto).

## 2. Buckets

El libro mayor guarda tres buckets separados y los consume en este orden:

| bucket | origen | vence | reembolsable |
|---|---|---|---|
| **free** | regalo de bienvenida (USD 12) y promociones | **60 días** después del abono (lo no usado se da de baja con una fila `expire`) | no |
| **bonus** | el extra de cada recarga (§ 9) | nunca | **no**; se **anula** si su recarga se reembolsa |
| **paid** | lo efectivamente pagado | nunca | sí, solo al medio de pago original |

- Un débito puede tocar varios lotes: se escribe una fila por lote
  consumido (`lot` apunta al lote). Cuando no queda ningún lote, el resto
  del débito es una fila *paid* sin lote: el sobregiro que lleva el saldo
  bajo cero hasta el piso de gracia (§ 3).
- **Reembolso** de una recarga: vuelve la porción pagada (`refund`, paid) y
  se anula el bonus de esa recarga (`void`, bonus) aunque ya se haya
  gastado → el saldo puede quedar negativo. Un reembolso parcial devuelve y
  anula en la misma proporción.
- **Inactividad**: 24 meses sin ningún movimiento en el libro → aviso por
  WhatsApp; 30 días después sin movimiento → se pierde todo el saldo
  (`forfeit` en cada bucket). Un movimiento posterior al aviso lo anula.
  Está en los términos (§ 4).

## 3. Saldo, gracia y modo manual

El saldo **nunca decide a qué clientes se atiende**. Por debajo de USD 2 el
agente sigue agendando todo.

| saldo total | estado | qué hace el agente | qué ve el dueño |
|---|---|---|---|
| ≥ 2.00 | `ok` | todo | franja verde con saldo y precios |
| 0.00 – 1.99 | `low` | todo | franja ámbar «Recarga pronto» |
| −4.00 – −0.01 | `grace` | todo (la cuenta puede quedar en −4.00) | franja roja «Saldo negativo» |
| < −4.00 | `manual` | **deja de responder**; cada mensaje pasa al dueño por la ruta de entrega manual existente (`attention`), el chat nunca se bloquea | franja roja y chip «Modo manual», **una** notificación con enlace de recarga |

Al pasar a `manual` la pasarela envía **un** WhatsApp al dueño con el enlace
de recarga; no se repite hasta que el saldo vuelva a ≥ 0 y caiga otra vez.
La app además muestra una alerta local (como máximo cada 6 h).

## 4. Términos del circuito cerrado

Texto que la app muestra bajo el botón de recarga (`credits_terms`) y en el
registro (`reg_terms_label`), ES/PT/EN; versión vigente **`2026-09`**:

> Los créditos solo se canjean por servicios de Agente. No son transferibles
> entre cuentas ni canjeables por dinero; los reembolsos se hacen únicamente
> al medio de pago original (el bonus de esa recarga se anula). Los créditos
> de regalo vencen a los 60 días; el bonus y los créditos pagados no vencen.
> Tras 24 meses sin actividad te avisamos y, 30 días después, el saldo se
> pierde. No son dinero, ni un depósito, ni una cuenta de pago.

La aceptación se guarda en la cuenta (`accounts.terms_version`,
`accounts.terms_accepted_at`): la app la envía en `POST /api/onboard_business`
(`termsVersion`, `termsAcceptedAt`), el núcleo la guarda en `businesses` y la
reenvía a la pasarela (`POST /v1/account/profile`).

## 5. Categoría del negocio y negocios prohibidos

El registro pide una categoría (`GET /api/categories`, lista embebida en la
app por si no hay red — `Categories.kt`, `gateway/categories.json`):

`peluqueria` · `estetica` · `consultorio` · `veterinaria` · `gimnasio` ·
`restaurante` · `ropa` · `servicios` · `otro`.

La pasarela también publica la lista **prohibida** (por defecto:
`farmacia_sin_receta`, `armas`, `apuestas`, `adulto`, `cripto_intercambio`,
`prestamos`). Esas opciones aparecen en el selector para que el gate pueda
actuar: elegir una muestra la pantalla neutra «Agente no está disponible
para este tipo de negocio» y el registro no continúa; el núcleo repite el
chequeo (`403 prohibited_category`) y la pasarela también
(`403 {"error":{"type":"prohibited"}}`). La categoría queda en
`accounts.category` y `businesses.category`.

## 6. Adelantos e impuestos por país

`depositsEnabled`, la tasa y la moneda de la pantalla salen de la tabla
`country_config` de la pasarela; **la app nunca lleva la lista de países**.
Con `depositsEnabled = false` el agente reserva sin pedir adelanto (aunque
haya `bookingDeposit`) y el panel muestra «Adelantos: próximamente en tu
país».

El impuesto se aplica **al dinero pagado**, nunca al bonus ni al regalo: en
cada fila de débito que sale del bucket *paid* se guarda `tax_rate` y
`net_cents = round(amount / (1 + rate))`; las filas *free* y *bonus* llevan
tasa 0 y neto = monto.

| país | tasa | | país | tasa |
|---|---|---|---|---|
| PE | 18 % | | AR | 21 % |
| MX | 16 % | | BR | 17 % (default) |
| CO | 19 % | | EC | 15 % |
| CL | 19 % | | PA | 7 % |
| otros | 0 % hasta que se fije | | | |

## 7. Datos (pasarela, `gateway/migrations/022_prepaid.sql`)

- `accounts.country` — ISO-2 del código de marcación del WhatsApp al crear
  la cuenta; **inmutable**. `accounts.category`, `terms_version`,
  `terms_accepted_at`.
- `country_config (iso, deposits_enabled, tax_rate, display_currency)`.
- `prepaid_ledger (id, account, kind, bucket, amount_cents, lot, expires_at,
  outcome_id, business, client_hash, is_new_client, tax_rate, net_cents,
  method, external_id, note, meta, created_at)`. `kind` ∈ `grant | promo |
  bonus | topup` (lotes, +) · `debit | reversal | refund | void | expire |
  forfeit`. Saldo = `SUM(amount_cents)`; por bucket, filtrando. Índice único
  `(kind, external_id)` = idempotencia de recargas y reembolsos.
- `prepaid_clients (account, business, client_hash, first_at)`.
- `prepaid_months (account, month, outcomes, charged_cents)`.
- `prepaid_welcome (phone)` — un regalo por número, para siempre.
- `prepaid_handoffs (account, notified_at)` — el aviso de modo manual.
- `prepaid_dormancy (account, warned_at, forfeited_at)`.

`client_hash = sha256("{business_id}:{dígitos del teléfono}")` se calcula en
el teléfono: la pasarela nunca ve el número del cliente.

En el núcleo (`server/migrations/014_outcomes.sql`): `businesses.category`,
`terms_*`, y la tabla `outcomes` (cola idempotente de resultados reportados
y reversiones pedidas) más `settings.credits_summary` (caché de la
pasarela).

## 8. Endpoints

### Pasarela (auth = agente firmado y vinculado, o sesión Yaya)

| método + ruta | cuerpo | devuelve |
|---|---|---|
| `GET /v1/credits` | | `{balance, currency:"USD", state, grace:-4, buckets:{free:{balance, expiresAt}, bonus:{balance}, paid:{balance}}, prices:{known:1,new:2,volumeAfter:100,volumeKnown:0.5,volumeNew:1,monthlyCap:199}, welcome:12, month:{outcomes, charged, capReached}, country, depositsEnabled, taxRate, displayCurrency, termsVersion, terms, dormancy, tiers:[{id,pays,credits,bonus}], topup:{presets, selected:"plus", methods:["card"\|"yape"], url}, ledger:[…50]}` |
| `POST /v1/outcomes/confirm` | `{outcomeId, business, clientHash, kind:"booking"\|"sale", customer?}` | idempotente: `{charged, isNewClient, balance, state, action?:"no_credits", topupUrl, month, depositsEnabled, duplicate?}` |
| `POST /v1/outcomes/reverse` | `{outcomeId}` | `{reversed, balance, state}`; `409` fuera de las 24 h |
| `GET /v1/wallets` | | `{version, wallets:[{package, name, countries}]}` (`WALLETS_FILE`) |
| `GET /v1/categories` | | `{categories, prohibited}` (`CATEGORIES_FILE`) |
| `POST /v1/account/profile` | `{category?, termsVersion?, termsAcceptedAt?}` | `403 prohibited` si corresponde |
| `POST /v1/topup/session` | `{tier:"basic"\|"plus"\|"max"}` (o `amount`) | `{url, tier, pays, credits, bonus}` — Dodo Checkout, tarjeta, moneda local seleccionable |
| `POST /v1/topup/yape` | `{tier}` (solo PE) | `{ref, amount, currency:"PEN", usdCents, igvInclusive, tier, credits, bonus, pay}` |
| `POST /v1/webhooks/dodo` | evento Standard Webhooks | `payment.succeeded` → lotes *paid* + *bonus*; `refund.succeeded` → `refund` + `void`; idempotente por id |
| `POST /admin/prepaid` (x-admin-key) | `{account\|phone, kind:"welcome"\|"promo", cents?, days?}` | abona el regalo o una promo al bucket *free* |

Regalo de bienvenida: en `POST /v1/accounts/otp/check` al crear la cuenta
con teléfono verificado; también `accounts.country` desde el código de
marcación.

### Núcleo (loopback, `docs/CORE-API.md`)

| método + ruta | notas |
|---|---|
| `GET /api/credits` | el resumen de arriba; `cached: true` cuando la pasarela no respondió |
| `GET /api/dashboard` → `credits` | `{balance, state, grace, buckets, prices, depositsEnabled, month, tiers, topup}` |
| `POST /api/execute_action` → `action: "no_credits"` | solo en `manual`: `agentResponse` vacío (la app no envía nada), `actionData:{manual:true, balance, topupUrl}`, `attention:[{kind:"no_credits", urgent:true, …}]` |
| `POST /api/topup/session` | `{tier, method:"card"\|"yape"}` |
| `GET /api/wallets` · `GET /api/categories` | proxies con caché de un día |
| `POST /api/onboard_business` | `category`, `termsVersion`, `termsAcceptedAt`; `403 {"error":"prohibited_category"}` |
| `POST /api/outcomes/{id}/reverse` | el dueño cancela/reembolsa |

Dónde cobra el núcleo (`outcomes::confirm`): al pasar una cita o un pedido
a `confirmed` — `book_appointment` (sin adelanto), el adelanto que llega por
`payment_event` o `collect_payment`, `create_order`. `handle_cancellation` y
`/api/outcomes/{id}/reverse` piden la reversión. Sin pasarela el cargo queda
en cola (`outcomes.synced = 0`) y se reintenta en el siguiente turno o
panel: el agente **no deja de agendar por falta de red**.

## 9. Recargas (tiers)

| tier | paga | recibe | paid | bonus |
|---|---|---|---|---|
| `basic` | USD 10 | USD 10.00 | 10.00 | 0.00 |
| `plus` (preseleccionado) | USD 25 | USD 27.50 | 25.00 | 2.50 |
| `max` | USD 50 | USD 57.50 | 50.00 | 7.50 |

- **Tarjeta (todos los países)**: Dodo Payments es merchant of record.
  Productos one-time en USD, `tax_inclusive`, creados con
  `agente.ceo/scripts/dodo-products.sh` (cuenta live: `basic`
  `pdt_0NmpOt5kcnKBABFi4Yedy`, `plus` `pdt_0NmpPMebmBFFNJC7fYtSg`, `max`
  `pdt_0NmpPMkAGYOZHBPYJFBbx`; webhook `ep_3IptX8OCYE0FOTjHsQGQ5nvR441` →
  `https://llm.yaya.tech/v1/webhooks/dodo`). El checkout permite pagar en
  moneda local (`allow_currency_selection`). La app abre la URL en una
  Custom Tab; el saldo llega por webhook y el dueño recibe un WhatsApp.
- **Yape/Plin (solo Perú)**: mismos tiers al tipo de cambio del día
  (`PEN_PER_USD`), monto en S/ IGV-inclusive, mismo bonus; la recarga
  existente verificada por notificación (yaya.cash) la acredita.
- Botones: «Recarga $25 → recibes $27.50». Perú ve «Tarjeta» y «Recargar
  con Yape»; el resto solo «Tarjeta».

Variables de entorno de la pasarela: `DODO_API_KEY`, `DODO_WEBHOOK_SECRET`,
`DODO_TIER_BASIC`, `DODO_TIER_PLUS`, `DODO_TIER_MAX`, `BILLING_RETURN_URL`,
`TOPUP_URL`, `PEN_PER_USD`, `WALLETS_FILE`, `CATEGORIES_FILE`. Sin secretos
en el código; en producción viven en `~/.agente/dodo.env`.

## 10. Migración desde 1.21

- Instalaciones «sin cuenta»: al abrir 1.22.0 van a iniciar sesión; el
  negocio y el device token sobreviven.
- `minVersionCode = 66`: la API del loopback cambió.
- Cuentas con plan vigente: `POST /admin/prepaid {kind:"promo"}` abona lo
  que quede como créditos *free* (criterio comercial). El regalo de USD 12
  es una sola vez por número de WhatsApp.

## 11. Verificación

- `cargo test` en `gateway/` (`prepaid::tests`): nuevo vs conocido por
  negocio e idempotencia; orden de consumo free → bonus → paid e impuesto
  solo en paid; vencimiento del regalo a los 60 días; mitad de precio en el
  101 y tope de 199; reversión a 24 h que vuelve a sus lotes; gracia → modo
  manual → recuperación; recargas idempotentes y reembolsos que anulan el
  bonus (saldo negativo); reembolso parcial proporcional; regla y
  ejecución de inactividad; firma de webhook.
- `cargo test` en `server/` (`outcomes::tests`): hash por negocio y
  canónico; lectura de `manual` y `depositsEnabled`.
- Guion manual `agente.ceo/scripts/credits-e2e.sh`: registrar → bienvenida
  12 → 7 resultados (mezcla) → gracia → entrega manual → pago de prueba Dodo
  → saldo restaurado. `--local` levanta una pasarela efímera y lo corre
  entero sin cuentas externas.
