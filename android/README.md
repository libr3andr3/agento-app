# agente

**Una recepcionista con IA que vive en el teléfono Android del dueño del negocio.**

[![ci](https://github.com/libr3andr3/agento-app/actions/workflows/ci.yml/badge.svg)](https://github.com/libr3andr3/agento-app/actions/workflows/ci.yml)

agente lee los mensajes que el teléfono del negocio ya recibe — WhatsApp,
WhatsApp Business, Instagram, Messenger, Telegram, SMS — a través del sistema
de notificaciones de Android, y los responde como el negocio: cotiza precios,
agenda citas, toma pedidos y confirma pagos (Yape, Plin, cualquier billetera o
banco que publique una notificación). Sin APIs de plataformas, sin scraping,
sin un servidor que guarde las conversaciones. El teléfono *es* el servidor.

Este repositorio es la app Android en producción ([agente.ceo](https://agente.ceo)):
el caparazón en Kotlin, sus pantallas y una copia precompilada del núcleo del
agente que las mueve — un runtime en Rust compuesto sobre el kernel de plugins
[corazón](https://github.com/libr3andr3/corazon). Está hecho para gente que
quiere entender, modificar y extender un agente real, en producción,
construido sobre notificaciones.

## Problemática

Los pequeños negocios del Perú atienden por WhatsApp, y la oferta actual de
«bots» para hacerlo les exige elegir entre tres riesgos:

1. **Entrega de credenciales.** La mayoría de las soluciones opera secuestrando
   una sesión de WhatsApp Web (código QR en un servidor ajeno) o pidiendo las
   claves de las plataformas. El negocio pierde el control de su propia
   identidad: quien tiene esa sesión puede leer y escribir como el negocio,
   desde cualquier lugar, sin que el dueño lo vea.
2. **Exfiltración de datos.** Las conversaciones con los clientes — datos
   personales protegidos por la Ley 29733 — se copian y almacenan en la nube
   de un tercero, fuera del alcance y de la jurisdicción del dueño.
3. **IA sin transparencia ni rastro.** Un modelo de lenguaje responde *en
   nombre del negocio* sin identificarse como IA (obligación de la Ley 31814,
   vigente desde enero de 2026) y sin dejar un registro verificable de qué
   dijo, cuándo y con qué herramientas.

agente ataca los tres problemas a la vez: el agente corre **dentro del
teléfono del negocio**, sin credenciales de plataformas ni sesiones remotas;
los datos **no salen a servidores de terceros**; y cada acción del agente
queda en una **cadena de auditoría firmada** verificable desde el propio
teléfono, con el agente presentándose como IA desde el primer mensaje.

## Solución y explicación

### Cómo funciona

```
  el cliente escribe por WhatsApp
          │
          ▼  Android publica una notificación
  ┌──────────────────────────────┐
  │ AgenteNotificationListener   │  lee remitente + texto (MessagingStyle)
  └──────────────┬───────────────┘
                 │ HTTP en 127.0.0.1  (ServerClient)
  ┌──────────────▼───────────────┐
  │ núcleo del agente            │  el agente: SQLite, herramientas, memoria
  │ libagente_core.so            │  ← solo la llamada al modelo de lenguaje
  │ (Rust, corre dentro del app) │    sale del teléfono (yaya.tech o tu propia clave)
  └──────────────┬───────────────┘
                 │ texto de respuesta
  ┌──────────────▼───────────────┐
  │ respuesta inline de la       │  el mismo botón «Responder» que ves en
  │ notificación (RemoteInput)   │  la bandeja de notificaciones
  └──────────────────────────────┘
```

La app usa el **acceso a notificaciones** de Android (`NotificationListenerService`)
como única integración con las apps de chat: lee la notificación entrante y
responde por la acción de respuesta inline de esa misma notificación — el
mismo botón «Responder» que usaría el dueño. No hay APIs de Meta, no hay
sesiones espejo, no hay automatización de UI.

### Funciones principales

- **Atención 24/7**: cotiza, agenda citas, toma pedidos y responde preguntas
  con los datos que el dueño le enseñó en una entrevista de dos minutos (por
  voz o texto; también extrae ítems de una foto de la carta o lista de precios).
- **Confirmación de pagos**: detecta en el teléfono los avisos de pago del
  banco o billetera del dueño (Yape, Plin y otras) y los asocia a la cita o
  pedido. No mueve dinero; solo lee notificaciones entrantes.
- **Panel del negocio**: Hoy (citas, pedidos, ingresos), Conversaciones,
  Clientes, Cobros, y exportación de todo (.vcf, .csv, .ics) — los clientes a
  los Contactos del teléfono y las citas a un calendario propio.
- **Créditos, no planes**: solo se cobra cuando se confirma una cita o venta —
  USD 1 si el cliente ya es conocido del negocio, USD 2 si es nuevo — y toda
  cuenta nueva recibe USD 12 de regalo. Responder, cotizar y confirmar pagos
  es gratis. Ver `docs/CREDITS.md`.
- **Apps y billeteras a elección**: al terminar la entrevista, el dueño
  decide en qué apps de chat responde el agente y de qué billeteras o bancos
  lee los avisos de pago (los de su país aparecen primero; cualquier otra app
  de dinero se reconoce igual). Todo se decide y se queda en el teléfono.
- **Auditoría verificable**: cada turno, herramienta ejecutada y pago leído
  queda en una cadena firmada (Ed25519) y encadenada por hash, con anclas
  contrafirmadas por la pasarela — verificable en el teléfono (Ajustes →
  Auditoría). Ver `docs/ARCHITECTURE.md` § «La cadena de auditoría».
- **Transparencia de IA (Ley 31814 / D.S. 115-2025-PCM)**: el primer mensaje
  al cliente se abre con «Hola, soy el asistente con IA de {negocio}» y el
  agente tiene prohibido hacerse pasar por una persona. Vive en el núcleo como
  el plugin `disclosure`, no como texto opcional que el dueño pueda quitar.

### El núcleo y el kernel corazón

`libagente_core.so` es el agente en sí: una biblioteca en Rust que corre
dentro del proceso de la app, guarda cada conversación, cita, pago y dato
aprendido en `filesDir/agente.db`, y expone una API HTTP pequeña en
`127.0.0.1:<puerto aleatorio>`. El lado Kotlin nunca toca la base de datos;
solo llama a esa API (`docs/CORE-API.md`).

El núcleo está compuesto sobre **[corazón](https://github.com/libr3andr3/corazon)**,
un kernel de plugins para runtimes de agentes en Rust, publicado aparte bajo
MIT/Apache-2.0. En corazón, todo lo que forma el runtime — herramientas,
adaptadores de modelo, hooks de ciclo de vida, el plugin `disclosure` — se
monta como un **plugin con nombre y procedencia**: cada registro lleva a su
dueño, los montajes son reversibles sin residuo, y `inspect()` devuelve la
tabla completa de montajes, que es la superficie sobre la que se construyen la
firma y la atestación de la cadena de auditoría. Los esquemas de datos que el
agente sabe pedir (campos base + bundles por vertical: peluquería, pollería,
taller…) viajan como assets en `assets/schemas/`.

El núcleo viene precompilado aquí para `arm64-v8a`, `armeabi-v7a` y `x86_64`;
`app/src/main/jniLibs/CORE.md` registra qué build está incluido (fecha,
commits de origen y sha256 por ABI).

### Alcance

- La app Android completa (este repo, AGPL): listener, pantallas, cliente del
  núcleo, detección de pagos, exportaciones, canal de actualización propio.
- Dos canales del mismo código: `direct` (APK con autoactualización en
  agente.ceo) y `play` (Google Play, paquete `yaya.tech.agento`, sin permisos
  de instalación ni de visibilidad de paquetes).
- Lanzamiento en toda Latinoamérica desde el día uno: la app va en
  español, portugués e inglés (sigue el idioma del teléfono o el que elija
  el dueño en Ajustes), y reconoce las billeteras y bancos del país con el
  que el dueño se registró (`Wallets.kt`).

### Limitaciones actuales

- **El código fuente del núcleo es privado**; aquí va precompilado. La
  superficie JNI y cada endpoint que la app usa están documentados
  (`docs/CORE-API.md`) para construir encima, y el kernel corazón sí es
  público. Es la limitación más señalada por colaboradores.
- **La llamada al modelo de lenguaje sale del teléfono** (a yaya.tech por
  defecto, o a cualquier endpoint compatible con OpenAI con tu propia clave).
  El turno que el modelo ve es lo único que viaja; ver «Qué sale del teléfono».
- **Depende del acceso a notificaciones**: sin ese permiso el agente está
  sordo, y los gestores de batería de algunos fabricantes matan el listener
  (la app pide la exención de optimización de batería).
- Solo responde apps de chat cuyas notificaciones exponen respuesta inline
  (`SupportedApps.kt` lista las probadas); llamadas de voz y notas de voz de
  clientes no se atienden.
- El emulador no tiene WhatsApp; ahí el agente se prueba por el chat del
  gerente en el panel.

## Seguridad (cero credenciales)

**Este repositorio no contiene ninguna credencial**: ni contraseñas, ni
tokens, ni claves de API, ni keystores. Está diseñado para que no pueda
contenerlas:

- **Firma de releases por variables de entorno.** El keystore de subida vive
  fuera del repo (`AGENTE_KEYSTORE`, `AGENTE_KEYSTORE_PASS`,
  `AGENTE_KEY_ALIAS`, `AGENTE_KEY_PASS`); sin esas variables el build compila
  igual pero sin firmar, así CI y colaboradores nunca las necesitan
  (`app/build.gradle.kts`, `docs/RELEASE.md`).
- **Cero credenciales de plataformas.** agente no pide ni almacena claves de
  WhatsApp/Meta/Telegram: su única integración es el permiso de notificaciones
  que el dueño concede en su propio teléfono.
- **Secretos del usuario cifrados en el dispositivo.** El token de sesión del
  dispositivo y la clave de API opcional del dueño se guardan cifrados con
  AES-GCM bajo el Android Keystore (`SecureStore.kt`) y quedan excluidos de
  los respaldos del sistema (`res/xml/data_extraction_rules.xml`).
- **Tráfico**: todo lo que sale va por TLS; el único tráfico en claro
  permitido es el loopback `127.0.0.1` hacia el núcleo
  (`res/xml/network_security_config.xml`).
- **Verificación de builds**: Play entrega la app firmada; el digest del
  firmante de la clave de subida está en `docs/RELEASE.md`.
- **Reportes**: las vulnerabilidades NO van en un issue público — ver
  `docs/SECURITY.md` § «Reportes».

### Qué sale del teléfono

- La llamada al modelo de lenguaje (medida, firmada con la clave propia del
  agente) — a `yaya.tech` por defecto o a tu propio endpoint.
- Voz y fotos del catálogo, para transcripción y extracción, por la misma vía.
- Tu cuenta Yaya (inicio de sesión con código de un solo uso por WhatsApp,
  saldo de créditos) y un respaldo cifrado de la cuenta.

Los mensajes de los clientes nunca se suben a ningún lado como tales; el
modelo ve el turno que se le pide responder. Ver `docs/SECURITY.md`.

## Inicio rápido

Necesitas un JDK 17+ y Android Studio (o solo el Android SDK). Nada más: ni
Rust, ni NDK, ni claves de API.

```bash
git clone https://github.com/libr3andr3/agento-app.git
cd agento-app/android
./gradlew assembleDebug
adb install -r app/build/outputs/apk/debug/app-debug.apk
```

Luego, en el teléfono:

1. Abre **agente** e inicia sesión con tu número de WhatsApp (código de un
   solo uso). La cuenta es obligatoria — no hay modo invitado — y al crearla
   recibes USD 12 en créditos.
2. Registra el negocio (nombre, rubro, teléfono del dueño).
3. Deja que la entrevista de configuración haga sus preguntas — por voz o
   escribiendo. También puedes fotografiar una carta o lista de precios y el
   núcleo extrae los ítems. Al final eliges en qué apps responde el agente y
   qué billeteras lee.
4. Concede el **Acceso a notificaciones** cuando lo pida. Ese permiso es el
   que hace que todo funcione; sin él, el agente está sordo.
5. Escríbele al teléfono del negocio desde otro teléfono. Mira llegar la
   respuesta.

Todo lo que el agente hace aparece en el panel y en
**Ajustes → Registro de actividad**.

## Dónde está cada cosa

```
app/src/main/
  AndroidManifest.xml          permisos (cada uno explicado), pantallas, el servicio listener
  java/tech/yaya/agente/
    AgenteApp.kt               Application: arranca el núcleo temprano, crea los canales de notificación
    AgenteCore.kt              puente JNI a libagente_core.so — start/stop, config, puerto
    ServerClient.kt            el ÚNICO cliente HTTP hacia el núcleo en loopback (reglas de hilos adentro)
    AgenteNotificationListener.kt  el corazón de la app: notificación entra → núcleo → respuesta inline sale
    PaymentDetector.kt         reenvía notificaciones de bancos/billeteras al núcleo (detección de pagos)
    UnknownAppObserver.kt      aprende apps de chat nuevas por la forma de sus notificaciones
    NotificationProfile.kt, ProfileStore.kt   el estado de prueba de una app aprendida
    Prefs.kt                   ajustes + estado local (SharedPreferences)
    SecureStore.kt             AES-GCM bajo el Android Keystore para los secretos en Prefs
    ReplyLog.kt                el registro de actividad (últimos 100 eventos)
    OwnerAlerts.kt             notificaciones locales cuando el agente necesita al dueño
    DeviceAttestation.kt       atestación de clave por hardware entregada al núcleo (mejor esfuerzo)
    LocationHelper.kt          ubicación aproximada → la ubicación pública del negocio
    OsSync.kt                  clientes a Contactos, citas al calendario, exportaciones
    Screens.kt                 qué pantalla es «home»
    ── pantallas ──
    WelcomeActivity.kt         launcher: un router sin UI
    AccountActivity.kt         inicio de sesión Yaya ID (código de un uso por WhatsApp)
    RegistrationActivity.kt    negocio → teléfono del dueño → código, paso a paso
    OnboardingActivity.kt      la entrevista de configuración y, después, el chat del gerente
    DashboardActivity.kt       home: estado, ingresos, agenda, conversaciones, preguntas abiertas
    MainActivity.kt            Ajustes: interruptor maestro, apps conectadas, registro, privacidad
    CrmListActivity.kt, ConversationActivity.kt   conversaciones y clientes
    PayoutActivity.kt          Cobros: a dónde te pagan los clientes
    CreditsActivity.kt         créditos: saldo, precios por cita, ledger, recarga
    AppsSetupActivity.kt       último paso de la entrevista: en qué apps responde y qué billeteras lee
    AuditActivity.kt           verificación de la cadena de auditoría en el teléfono
    Countries.kt, CountryPicker.kt, SupportedApps.kt, Wallets.kt   catálogos pequeños (países, apps de chat, billeteras por país)
    AppToggles.kt, AppLanguage.kt   filas de interruptor por app; idioma de la app (es / pt / en)
  res/                         layouts, strings (values = español, values-pt = portugués, values-en = inglés), tokens de diseño
  assets/schemas/              lo que el agente sabe preguntar: campos base + bundles por vertical
  jniLibs/<abi>/libagente_core.so   el núcleo del agente (ver jniLibs/CORE.md)
docs/                          ARCHITECTURE, CORE-API, CREDITS, DESIGN, SECURITY, RELEASE, PLAY
```

Empieza por `AgenteNotificationListener.kt` — léelo de arriba abajo y
entiendes el producto. Luego `ServerClient.kt` para ver cómo la app habla con
el núcleo, y después `docs/ARCHITECTURE.md`.

## Compilar un release

```bash
export AGENTE_KEYSTORE=~/.agente/upload-keystore.jks \
       AGENTE_KEYSTORE_PASS=… AGENTE_KEY_ALIAS=agente-upload AGENTE_KEY_PASS=…
./gradlew bundleRelease             # → app/build/outputs/bundle/release/app-release.aab (docs/PLAY.md)
```

Procedimiento completo y numeración de versiones:
`docs/RELEASE.md`; el envío a Google Play: `docs/PLAY.md`.

## Buscamos colaboradores

agente es software libre en producción y buscamos gente que quiera meterle
mano. Nos sirven especialmente:

- **Android/Kotlin** — pantallas, el listener, compatibilidad con más
  fabricantes y apps de chat.
- **Seguridad** — revisión del modelo de amenazas, del almacenamiento de
  secretos, de la cadena de auditoría y de la atestación; toda mirada con
  formación en ciberseguridad es bienvenida (los límites de confianza están
  documentados en `docs/ARCHITECTURE.md` y `docs/SECURITY.md`).
- **Verticales** — los esquemas y `skill.md` por rubro en `assets/schemas/`:
  enseñarle al agente cómo funciona una peluquería, una pollería, un taller.
- **Traducciones** — los textos de usuario viven en `res/values` (español) y
  `res/values-en` (inglés).

Abre un issue o un pull request; las reglas están en `CONTRIBUTING.md`.

## Licencia

[GNU AGPL v3](LICENSE). agente es software libre: puedes ejecutarlo,
estudiarlo, modificarlo y redistribuirlo, y cualquiera a quien le des un
build — o a quien se lo sirvas por red — recibe los mismos derechos y el
código fuente. Las contribuciones se aceptan bajo la misma licencia.
El kernel [corazón](https://github.com/libr3andr3/corazon) se publica aparte
bajo MIT/Apache-2.0.

## Idioma y estado

La app va en español (idioma por defecto), portugués e inglés. La
documentación está en español; los identificadores y comentarios del código
están en inglés. Esta es la app en producción en
[agente.ceo](https://agente.ceo). El paquete Kotlin sigue siendo
`tech.yaya.agente` (el primer nombre de la app) — no se puede renombrar sin
recompilar el núcleo, cuyos puntos de entrada JNI llevan ese nombre.
