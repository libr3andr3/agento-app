# Seguridad — qué guarda la app y quién puede llegar a ello

## Secretos en el teléfono

| secreto | dónde | por qué |
|---|---|---|
| device token | `Prefs` → `SecureStore` (AES-256-GCM, clave en el Android Keystore) | autoriza cada llamada de negocio al núcleo; no expira |
| app key del núcleo | `SecureStore` | aleatorio por instalación; el núcleo rechaza peticiones de loopback sin él |
| KEK de identidad | `SecureStore` (32 bytes) | el núcleo sella su semilla Ed25519 bajo él — una copia de `agente.db` sin el teléfono no es la identidad del negocio |
| clave LLM propia | `SecureStore` | solo si el dueño trajo su propio modelo |
| sesión Yaya, clave de respaldo | dentro de `agente.db`, selladas por el núcleo | |

`SecureStore` son ~100 líneas: AES/GCM sobre una clave del Keystore, sin
requisito de autenticación de usuario (el listener corre con el teléfono
bloqueado). Una clave del Keystore perdida por una rareza del SO descifra a
`null`, lo que significa re-emparejar — nunca un fallback en claro.

Los respaldos del sistema están apagados (`allowBackup=false` +
`data_extraction_rules.xml`). El HTTP en claro solo se permite en builds de
debug (`network_security_config.xml`) para un núcleo o pasarela alcanzados
por `adb reverse`. El núcleo en loopback es HTTP plano por diseño — nunca
cruza la frontera del proceso.

## Permisos, y para qué es cada uno

| permiso | por qué |
|---|---|
| `BIND_NOTIFICATION_LISTENER_SERVICE` (se concede en ajustes del sistema) | el producto: leer notificaciones de chat, responder inline |
| `POST_NOTIFICATIONS` | avisarle al dueño cuando el agente lo necesita, y de actualizaciones |
| `INTERNET`, `ACCESS_NETWORK_STATE` | las llamadas del núcleo a la pasarela; el texto «sin conexión» |
| `RECORD_AUDIO` | la entrevista por voz (el audio va al núcleo → transcripción; nunca se guarda) |
| `ACCESS_COARSE_LOCATION` | un distrito para «cerca de mí»; se pide una vez, con explicación |
| `REQUEST_INSTALL_PACKAGES` | el canal de actualización propio |
| `REQUEST_IGNORE_BATTERY_OPTIMIZATIONS` | los mata-apps de batería de los fabricantes detienen el listener; pedimos la exención |
| `QUERY_ALL_PACKAGES` + `<queries>` | Cobros verifica si la billetera que el dueño nombró está instalada; Ajustes muestra qué apps de chat hay |

El listener ve todas las notificaciones del teléfono. Lo que la app hace con
ellas: apps de chat que el dueño activó → al núcleo; cualquier cosa con
respuesta inline de una app desconocida → solo la *forma*
(`UnknownAppObserver`), nunca el contenido; billeteras y bancos que el dueño
dejó encendidos → reenviados crudos al núcleo *en este teléfono* para
detección de dinero, y silenciados cuando el núcleo dice «aquí no hay
dinero». Nada se envía a ningún otro servidor.

Cualquier **otra** app solo llega al núcleo si el dueño la enciende en «¿Qué
avisos puede leer tu agente?», o enciende «Apps que instales después», que
desde 1.30.0 **viene apagado** (las instalaciones que ya tenían negocio y
nunca respondieron conservan su valor anterior). Y hay fuentes que nunca se leen
como pago, encendido o no: la app de SMS predeterminada y los clientes de
correo (`PaymentDetector.NEVER_MONEY`). El texto de esas notificaciones lo
escribe quien le escribe al dueño, no la app: un SMS con remitente falsificado
o el asunto de un correo que diga «Yape: recibiste S/ 200» costaba nada y
llegaba al agente como candidato a pago.

## Qué llega a yaya.tech

- Llamadas de LLM, voz y visión, firmadas con la clave del agente. El prompt
  contiene lo que el agente necesita para responder el turno actual.
- Llamadas de cuenta y créditos (saldo, ledger); respaldos cifrados de la cuenta.
- La tarjeta pública del agente (nombre del negocio, oferta, ubicación,
  atestación).

La pasarela guarda metadatos (qué agente, cuándo, cuánto). No puede leer el
tráfico del relay entre agentes: son cajas selladas.

## Atestación por hardware

`DeviceAttestation` genera una clave P-256 en el TEE/StrongBox con un desafío
de atestación ligado al id del agente y entrega la cadena de certificados al
núcleo, que la publica con la tarjeta. El registro la verifica contra las
raíces de Google y fija (pin) el certificado de firma de esta app
(`EXPECTED_SIGNERS`). Los teléfonos sin atestación quedan simplemente como
«no verificados».

## Transparencia ante el cliente (Ley 31814)

El núcleo monta un plugin de comportamiento (`disclosure`) que obliga al
agente a abrir el primer mensaje que un cliente recibe presentándose como el
asistente con IA del negocio, y le prohíbe hacerse pasar por una persona en
cualquier turno. Es parte del prompt compuesto en cada turno de cliente, no
una opción de configuración. La cadena de auditoría (ver
`docs/ARCHITECTURE.md`) conserva evidencia firmada de cada respuesta enviada
en nombre del negocio.

## Reportes

Problemas de seguridad: escribe a la dirección de contacto en
https://agente.ceo/privacy.html. Por favor no abras un issue público para una
vulnerabilidad.
