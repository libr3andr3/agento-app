# Envío a Google Play

La app es una sola: paquete **`yaya.tech.agento`**, la registrada en Play
Console. Lo que Play no permite o hace por su cuenta no está en el build:

- **Actualizaciones:** las entrega Play; no hay canal propio ni
  `REQUEST_INSTALL_PACKAGES`.
- **`QUERY_ALL_PACKAGES`:** no. Las billeteras comunes y la lista de apps
  (pantalla «¿Qué avisos puede leer?», intent MAIN/LAUNCHER) van en
  `<queries>`, que no necesita declaración.
- **Créditos:** sin recarga en la app ni enlace a una (política de pagos de
  Play); se recargan desde la cuenta Yaya (`docs/CREDITS.md`).

Compilar y verificar:

```bash
set -a; . ~/.agente/keystore.env; set +a   # AGENTE_KEYSTORE / _PASS / _ALIAS / KEY_PASS
./gradlew bundleRelease assembleRelease
# app/build/outputs/bundle/release/app-release.aab   ← esto se sube
# app/build/outputs/apk/release/app-release.apk      ← adb install para una pasada de cordura
jarsigner -verify app/build/outputs/bundle/release/app-release.aab
$ANDROID_HOME/build-tools/<ver>/aapt2 dump badging app/build/outputs/apk/release/app-release.apk | grep -E "^package|uses-permission"
```

Permisos esperados en el build de Play (verificado 1.23.2/70): `POST_NOTIFICATIONS`,
`INTERNET`, `ACCESS_NETWORK_STATE`, `RECORD_AUDIO`, `ACCESS_COARSE_LOCATION`,
`REQUEST_IGNORE_BATTERY_OPTIMIZATIONS`, `READ/WRITE_CONTACTS` (clientes en la
agenda del teléfono), `READ/WRITE_CALENDAR` (citas en el calendario del dueño),
más el servicio del listener de
notificaciones (`BIND_NOTIFICATION_LISTENER_SERVICE` en el servicio, no un
`uses-permission`).

## Checklist de Play Console

**App content → Permissions declaration.** El listener es el producto; Play
pide una justificación y un video. El texto siguiente se pega tal cual en la
consola (en inglés):

> *Notification listener (core functionality):* agento is a receptionist
> for a business phone. Right after setup the owner chooses, one switch
> per app, which apps' notifications it may read (the business's WhatsApp
> and the owner's wallets and banks are suggested; every other app starts
> off), and then, among the chat apps it reads, which ones it answers on,
> through the notification's own inline reply. Notifications from apps
> left off are discarded on the device without being read; both lists can
> be changed at any time in Settings. Without notification access the app
> has no function. Of what it reads, only the text the agent needs leaves
> the phone, sent to our model gateway: the customer message it answers,
> or a payment notice it checks for an incoming payment. Video: install →
> sign in → register → grant "Notification access" → choose the apps it
> reads → choose the apps it answers on → a WhatsApp message arrives on
> the phone → the reply is sent from the notification.

- `RECORD_AUDIO`: respuestas por voz en la entrevista de configuración (el
  audio se transcribe, no se guarda). `ACCESS_COARSE_LOCATION`: distrito para
  «cerca de mí» (se pide una vez, explicado en la app).
  `REQUEST_IGNORE_BATTERY_OPTIMIZATIONS`: el listener muere con los gestores
  de batería de los fabricantes; la app le pide al dueño la exención.

**App content → Data safety** (qué declarar):

| dato | se recolecta | se comparte | propósito | notas |
|---|---|---|---|---|
| Nombre, correo, teléfono | sí | no | gestión de cuenta | inicio de sesión Yaya ID (código de un uso) |
| Mensajes (chats de clientes) | sí, procesados | con nuestro proveedor de IA como encargado | funcionalidad | el mensaje del cliente se envía al modelo para producir la respuesta; se guarda en el teléfono, no en nuestros servidores |
| Audio | sí, procesado | no | funcionalidad | notas de voz de la entrevista, transcritas, no retenidas |
| Ubicación aproximada | sí | no | funcionalidad | opcional |
| IDs de dispositivo u otros | sí | no | funcionalidad | clave de agente por instalación + atestación por hardware |
| Fotos | sí, procesadas | no | funcionalidad | foto opcional del catálogo → ítems extraídos |
| Contactos | sí | no | funcionalidad | opcional: guardar clientes en la agenda del teléfono; se quedan en el teléfono |
| Calendario | sí | no | funcionalidad | opcional: citas en el calendario del dueño; se quedan en el teléfono |

Cifrado en tránsito: sí. Eliminación: en la app («Cerrar sesión» /
desinstalar borra los datos del teléfono; eliminación de cuenta en https://yaya.tech/eliminar-cuenta o por correo al
contacto de privacidad). Política de privacidad:
https://agente.ceo/privacidad.html — Términos: https://agente.ceo/terminos.html.

- **Anuncios**: ninguno. **Público objetivo**: 18+ (dueños de negocio).
  **Categoría**: Empresa. **Clasificación de contenido**: utilidad, sin
  contenido público generado por usuarios.
- **App access**: no hay modo invitado — toda cuenta se crea con un código
  por WhatsApp. Provee en *App access → instructions* un teléfono de prueba
  que reciba WhatsApp (o un número con código fijo configurado en la
  pasarela) y cualquier nombre de negocio; el paso de verificación del
  número del negocio se salta cuando el servidor responde 503.
- **Funciones financieras**: ninguna (la app detecta las notificaciones de
  pago entrantes del dueño; no mueve dinero).

## Ficha de la tienda (es-PE por defecto, en-US segundo)

**Nombre**: `agente`

**Corto (es)**: `Tu recepcionista con IA: responde tus chats, agenda citas y confirma pagos.`
**Corto (en)**: `Your AI receptionist: answers your chats, books appointments, confirms payments.`

**Completo (es)**

agente es la recepcionista con IA de tu negocio. Vive en tu teléfono y responde por ti en WhatsApp Business, Instagram, Messenger y Telegram: cotiza, agenda citas, toma pedidos y confirma pagos, las 24 horas, mientras tú trabajas.

CÓMO FUNCIONA
• Una entrevista de dos minutos, por voz o escrita, le enseña qué vendes, tus precios y tus horarios. También puedes tomarle una foto a tu carta o lista de precios.
• Lee las notificaciones de las apps de chat que tú actives y contesta desde la misma notificación, como lo harías tú.
• Detecta los avisos de pago de tu banco o billetera (Yape, Plin y otros) y los anota en la cita o el pedido.
• Cuando no sabe algo, te pregunta a ti; tu respuesta la aprende al instante.

TU NEGOCIO, EN TU MANO
• Hoy: citas, pedidos e ingresos del día.
• Conversaciones y clientes: qué dijo cada quien y qué hizo tu agente.
• Cobros: tú decides a dónde te pagan.
• Panel web en agente.ceo para verlo desde la computadora.

PRIVACIDAD PRIMERO
• Tus conversaciones y los datos de tu negocio viven en tu teléfono, no en nuestros servidores.
• Solo lee notificaciones de las apps que tú elijas, y solo después de que le des el permiso.
• Código abierto (AGPL): github.com/libr3andr3/agento-app

Empiezas con US$ 12 de créditos de bienvenida. Solo pagas por resultados: US$ 1 por cada cita o venta confirmada con un cliente que ya conocías y US$ 2 con uno nuevo; la mitad después de 100 al mes y nunca más de US$ 199 mensuales. Sin suscripción ni permanencia. Los créditos se gestionan desde tu cuenta Yaya.
agente es de Yaya Tech PBC.

**Completo (en)**

agente is your business's AI receptionist. It lives on your phone and answers for you on WhatsApp Business, Instagram, Messenger and Telegram: it quotes, books appointments, takes orders and confirms payments, 24 hours a day, while you work.

HOW IT WORKS
• A two-minute interview, spoken or typed, teaches it what you sell, your prices and your hours. You can also photograph your menu or price list.
• It reads the notifications of the chat apps you enable and replies from the notification itself, the way you would.
• It detects payment alerts from your bank or wallet and records them against the booking or order.
• When it doesn't know something it asks you; your answer is learned on the spot.

YOUR BUSINESS, IN YOUR HAND
• Today: appointments, orders and earnings.
• Conversations and customers: what each one said and what your agent did.
• Payouts: you decide where customers pay you.
• Web console at agente.ceo for the desktop.

PRIVACY FIRST
• Your conversations and business data live on your phone, not on our servers.
• It only reads notifications from the apps you choose, and only after you grant access.
• Open source (AGPL): github.com/libr3andr3/agento-app

You start with US$ 12 in welcome credits. You only pay for results: US$ 1 per confirmed booking or sale with a customer you already had, US$ 2 with a new one; half price after 100 a month and never more than US$ 199 a month. No subscription, no commitment. Credits are managed from your Yaya account.
agente is made by Yaya Tech PBC.

**Capturas**: teléfono, 9:16, mínimo cuatro — inicio de sesión, la
entrevista, Hoy, Conversaciones, Cobros, Ajustes. Feature graphic 1024×500,
icono 512×512 (`app/src/main/res/mipmap-anydpi-v26` es la fuente adaptativa).

**Notas de versión (es)**: `Primera versión en Google Play de agente para negocios.`

## Adiciones D17 (1.18.0)

Permisos: `READ_CONTACTS`, `WRITE_CONTACTS`, `READ_CALENDAR`,
`WRITE_CALENDAR` — pedidos en tiempo de ejecución solo cuando el dueño activa
«Guardar clientes en Contactos» / «Guardar citas en el Calendario». Data
safety: los contactos y eventos de calendario se escriben en el dispositivo
(los propios clientes y citas del dueño) y nunca se recolectan ni
transmiten; la app lee Contactos solo para no duplicar lo que escribió.
Texto de la función: «Tus clientes se quedan contigo: en tus Contactos y tu
Calendario, y exportables (.vcf, .csv, .ics).»
