# Arquitectura

## Una sola idea

El teléfono del negocio es el servidor. Todo lo que el agente sabe —
conversaciones, citas, pedidos, pagos, la configuración aprendida del
negocio, la identidad criptográfica del propio agente — vive en un archivo
SQLite en el almacenamiento privado de la app. Lo único que sale del teléfono
son las llamadas al modelo de lenguaje y la fontanería de la cuenta. No hay
un «backend que guarda los datos del negocio»; una brecha en nuestros
servidores no es una brecha del negocio.

## Tres partes

```
┌────────────────────────── el teléfono ─────────────────────────┐
│                                                                │
│  caparazón Kotlin (este repo)      núcleo (libagente_core)     │
│  ───────────────────────           ─────────────────────────── │
│  NotificationListenerService  ──►  HTTP 127.0.0.1:<puerto>     │
│  pantallas, ajustes, alertas  ◄──  respuestas JSON             │
│  SharedPreferences            │    SQLite  filesDir/agente.db  │
│  Android Keystore             │    esquemas filesDir/schemas/  │
│                               │    identidad Ed25519 (sellada) │
└───────────────────────────────┼────────────────────────────────┘
                                │ HTTPS, peticiones firmadas por la identidad
                       ┌────────▼────────┐
                       │  yaya.tech      │  proxy medido de LLM / voz / visión
                       │  pasarela       │  cuentas Yaya ID, créditos, respaldos
                       └─────────────────┘  un relay que no puede leer el correo
```

**Caparazón Kotlin** — todo lo Android: el listener de notificaciones, las
pantallas, las preferencias, el canal de actualización. No tiene lógica de
negocio propia; ante la duda, le pregunta al núcleo.

**Núcleo del agente** — una biblioteca Rust cargada con
`System.loadLibrary("agente_core")` (`AgenteCore.kt`). `start(configJson)`
arranca un servidor HTTP en un puerto loopback aleatorio y devuelve el
puerto; el lado Kotlin le sigue hablando HTTP plano. El núcleo ejecuta el
bucle del agente (prompt, herramientas, memoria), mantiene la base de datos,
firma las peticiones salientes y habla con la pasarela.

**Pasarela** — `https://agente.ceo` / `llm.yaya.tech`. Desde el punto de
vista de la app es (a) a dónde el núcleo manda las llamadas de LLM, (b) donde
vive la cuenta Yaya, (c) donde quedan los respaldos cifrados. La app solo
llama al núcleo; el núcleo llama a la pasarela. Las actualizaciones las
entrega Google Play.

## Arranque

1. `AgenteApp.onCreate` → `AgenteCore.ensureStarted()` en un hilo de fondo.
2. `ensureStarted` copia `assets/schemas/` a `filesDir/schemas/` una vez por
   `versionCode`, construye la config (ruta de la base, directorio de
   esquemas, dos secretos aleatorios por instalación desde `SecureStore`,
   endpoint LLM propio opcional) y llama al `start` de JNI.
3. El núcleo devuelve su puerto. `AgenteCore.baseUrl()` es
   `http://127.0.0.1:<puerto>` desde entonces; cada llamada de `ServerClient`
   va ahí con `X-App-Key: <secreto por instalación>`.
4. `DeviceAttestation.ensure()` corre una vez por id de agente: una clave
   P-256 atestada por el Keystore cuya cadena de certificados el núcleo
   adjunta a la tarjeta pública del agente (mejor esfuerzo; la mayoría de
   teléfonos dicen «TEE», algunos no dicen nada).

El servicio listener (`onListenerConnected`) también llama a
`ensureStarted`, así que el núcleo está arriba incluso cuando la UI de la
app nunca se abrió después de un reinicio.

## Un mensaje de cliente, de punta a punta

`AgenteNotificationListener.process()`:

1. Interruptor maestro apagado (`Prefs.isEnabled`) → ignorar todo.
2. ¿El paquete es una app de chat conocida (`SupportedApps`) o una que este
   teléfono aprendió (`ProfileStore`)? Si ninguna: pasársela a
   `UnknownAppObserver` (podría ser una app de chat aprendible) y luego a
   `PaymentDetector` (podría ser dinero). Listo.
3. Saltar resúmenes de grupo y notificaciones en curso. Parsear el mensaje —
   `MessagingStyle` primero (remitente, texto, bandera de grupo, «de uno
   mismo»), título/texto plano como fallback.
4. Descartar nuestros propios mensajes en eco, chats de grupo salvo que estén
   habilitados, y reenvíos de una notificación ya manejada (ventana de
   identidad de 10 minutos).
5. Encontrar la acción de respuesta inline (`RemoteInput`). ¿No hay acción de
   respuesta? → registrarlo, listo.
6. **Modo agente** (hay un negocio registrado): `POST /api/execute_action` en
   el executor de un solo hilo (`ServerClient.EXECUTOR`), para que las
   respuestas a una conversación mantengan el orden. El núcleo contesta con
   `agentResponse` más un `action` opcional y `attention[]` (preguntas que no
   pudo responder, o un cliente pidiendo un humano → `OwnerAlerts`). La
   respuesta se envía por el `PendingIntent` de la propia notificación.
   **Modo fijo** (aún no hay negocio): enviar el texto de respuesta fijo con
   un cooldown por conversación.
7. Cada desenlace queda en `ReplyLog` — la prueba del dueño de qué se dijo en
   su nombre.

Si el núcleo no responde, sale la respuesta fija — el cliente siempre escucha
algo.

## Un pago

Las billeteras y bancos nunca están listados en la app. `PaymentDetector`
reenvía la notificación *cruda* (título, texto, canal, plantilla,
instalador…) de cualquier app que no sea de chat a `POST /api/payment_event`;
el núcleo decide «dinero / no dinero», lo cruza con una cita o pedido
pendiente, y puede contestar `mute` para un paquete que nunca trae dinero.
Los paquetes que la red ya conoce como apps de dinero en este país
(`GET /api/payment_sources`) nunca se silencian. El soporte de una billetera
nueva es un teléfono viéndola primero.

## Aprender una app de chat nueva

Cualquier notificación con respuesta inline es una conversación. Para un
paquete desconocido, `UnknownAppObserver` guarda la *forma* de tres
notificaciones distintas (¿MessagingStyle o no?, ¿título+texto o no?, ¿el
parser compartido la leyó?) — nunca el contenido. Tres formas consistentes y
parseables → se monta un `NotificationProfile` en modo **sombra**: el
listener lee esa app y lo registra pero no contesta. Tras diez lecturas
limpias se le ofrece al dueño un interruptor en Ajustes; si las lecturas
empiezan a fallar el perfil se pausa solo; dos pausas y se descarta por una
semana. Todo esto es datos en `ProfileStore`; desmontarlo no deja nada
atrás.

## Primer arranque

```
WelcomeActivity (router, sin UI)
  sin identidad Yaya  ──► AccountActivity      nombre/correo/teléfono → código de un uso
  sin negocio         ──► RegistrationActivity negocio → teléfono del dueño → código → /api/onboard_business
  sin entrevista aún  ──► OnboardingActivity   entrevista por voz/texto; foto del catálogo; «¡Todo listo!»
                                               exige el Acceso a notificaciones antes de dejarte salir
  si no               ──► DashboardActivity
```

`Prefs.serverConfigured()` (existe un device token) es lo que «hay un negocio
registrado» significa en toda la app.

## La app del dueño es datos (D15)

`DashboardActivity` no dibuja nada propio debajo del encabezado. El núcleo
compone un **spec de UI** para el negocio — la plantilla `ui:` del bundle de
su vertical (`assets/schemas/bundles/<vertical>/bundle.yml`) ⊕ el diseño que
el agente de onboarding guardó con su herramienta `design_ui` — y la app lo
renderiza: una pestaña de la barra inferior por entrada, cada pestaña una
columna de bloques del **catálogo de bloques** fijo en `Blocks.kt`
(`earnings`, `attention`, `orders_board`, `agenda_day`, `agenda_week`,
`catalog`, `conversations`, `contacts`). Un restaurante abre en su tablero de
pedidos, un salón en las citas de hoy, un puesto de Gamarra en su catálogo;
el dueño puede decirle al agente «quiero ver la agenda primero» y el próximo
refresh redibuja. `Walkthrough.kt` muestra las pestañas una vez, con las
palabras del propio agente (`intro`). El spec se valida en el núcleo
(`ui.rs`): bloques desconocidos se descartan, bloques que el negocio no puede
usar se descartan, máximo cuatro pestañas, la plantilla es el fallback — la
app nunca ve un spec que no pueda dibujar.

La vertical también trae un `skill.md`: cómo funciona ese tipo de negocio y
cómo entrevistar a su dueño (orden de preguntas, cómo una respuesta guía la
siguiente). El núcleo lo monta en el prompt de onboarding y su sección de
clientes en el prompt de cliente.

Las fotos viven en el teléfono (tabla `media`); los clientes las ven por un
enlace privado que la pasarela sirve por cinco minutos
(`privado.yaya.tech/…`).

## La cadena de auditoría

`audit_log` es el registro a prueba de manipulación, del propio teléfono, de
lo que el agente hizo: cada turno de cliente y de dueño (las palabras del
cliente como digests, la respuesta del agente literal), cada llamada de
herramienta, cada comando que entró por el relay del dueño desde la consola
web, cada pago leído, cada swipe, cada enlace privado, cada restauración y
arranque. Cada fila está encadenada por hash a la anterior y firmada con la
identidad Ed25519 de la instalación; UPDATE y DELETE se rechazan por trigger;
los timestamps nunca retroceden; y cada 25 entradas o 15 minutos la cabeza se
**ancla** — la pasarela contrafirma `(agente, seq, head, su propio reloj)` y
ambos lados guardan el ancla — de modo que una fila queda probadamente entre
dos instantes confiables sin importar qué diga el reloj del teléfono.
Ajustes → Auditoría verifica la cadena completa en el teléfono.

## Tus datos, en tu teléfono (D17)

Con el consentimiento del dueño (Ajustes → «Tus datos, en tu teléfono»),
`OsSync.kt` refleja el CRM en el SO después de cada refresh del panel: los
clientes con número a Contactos (etiquetados con una fila MIME propia que
lleva el id del CRM, así nunca duplica), las citas próximas a un calendario
local «agente» (con clave `CUSTOM_APP_URI`, recordatorio una hora antes,
borrado al cancelar). Los mismos datos salen como archivos: `.vcf` / `.csv`
desde Clientes, `.ics` desde la vista semanal. Unidireccional, idempotente,
nunca se envía a ningún lado.

## Hilos

`ServerClient` documenta el contrato; respétalo:

- `EXECUTOR` — un hilo. Turnos de conversación (`execute_action`,
  `onboarding_message`, `voice_message`, `catalog_photo`, registro). El orden
  importa; un turno puede tomar hasta tres minutos.
- `IO_EXECUTOR` — pool cacheado. Panel, pagos, respuestas a gaps, cuenta,
  CRM. Esto nunca debe hacer cola detrás de un turno lento de LLM: una
  notificación de Yape esperando detrás de un turno de chat parece un pago
  perdido.
- Nada de `ServerClient` puede correr en el hilo principal.

Los reintentos son opt-in por endpoint (`Retry.NEVER / IDEMPOTENT /
NETWORK_FAILURE_ONLY`) porque la mayoría de endpoints tienen efectos
secundarios.

## Almacenamiento

| dónde | qué |
|---|---|
| `filesDir/agente.db` | el SQLite del núcleo: conversaciones, citas, pedidos, pagos, contactos, el esquema por capas del negocio, la identidad sellada del agente, la sesión de la cuenta |
| `filesDir/schemas/` | copia de `assets/schemas` (campos base + bundles por vertical), refrescada por versión de la app |
| SharedPreferences `agente_prefs` (`Prefs`) | interruptor maestro, toggles por app de chat y por app de dinero, respuesta fija, cooldown, idioma, panel cacheado, transcripción del chat, estado del canal de actualización; entradas **cifradas**: device token, clave LLM propia, el app key del núcleo y el KEK de identidad |
| `agente_log` (`ReplyLog`) | últimos 100 eventos del registro de actividad |
| `agente_observer`, `ProfileStore` | formas en observación, perfiles aprendidos |
| Android Keystore | la clave AES con la que `SecureStore` envuelve los secretos; la clave de atestación |

El respaldo del sistema está deshabilitado (`allowBackup=false`,
`data_extraction_rules`) porque el token y las conversaciones de clientes no
deben viajar en un respaldo del dispositivo. Toda cuenta con sesión tiene en
su lugar un respaldo *cifrado* a través del núcleo (`POST /api/backup`,
`POST /api/restore`).

## Fronteras de confianza

- **App ↔ núcleo**: `X-App-Key`, un secreto aleatorio por instalación, para
  que otra app del teléfono no pueda hablarle al núcleo;
  `Authorization: Bearer <device token>` nombra al negocio en las rutas de
  negocio.
- **Núcleo ↔ pasarela**: cada petición firmada con la clave Ed25519 del
  agente (`X-Agent-Auth`); la semilla está sellada en reposo bajo una clave
  envuelta por el Keystore.
- **Pasarela**: ve quién habla con quién y cuándo (metadatos), nunca un
  mensaje de cliente. El relay solo transporta cajas selladas.

## Sistema de diseño

Cada pantalla se construye con los tokens de
`res/values/{colors,design,themes}.xml` y la fuente Plus Jakarta Sans. Lee
`docs/DESIGN.md` antes de tocar un layout.
