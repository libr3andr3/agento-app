# Contribuir

## Compilar

`./gradlew assembleDebug` — eso es todo. JDK 17+, Android SDK 36 (Android
Studio lo instala). El núcleo del agente (`../server`) se compila aparte con
`cargo ndk` para arm64 y armv7 (ver `docs/RELEASE.md`); sin él la app compila
y los tests corren, pero el agente no arranca en un teléfono.

El CI compila, corre los tests unitarios y `lintDebug` en cada push y pull
request.

## Licencia

Al contribuir aceptas que tus cambios se publican bajo la
[AGPL-3.0](LICENSE), como el resto del proyecto.

## Reglas de la casa

- **Lee `docs/DESIGN.md` antes de tocar un layout.** Solo tokens: nada de
  colores o tamaños en duro, ni estilos `Widget.Material3.*` en los layouts.
- **Strings**: español en `res/values/strings_<area>.xml`, inglés en
  `res/values-en/strings_<area>.xml`. Todo texto de usuario existe en ambos.
  El español es peruano informal («tú», «citas», «cobros»).
- **Red**: solo a través de `ServerClient`, en el executor que su comentario
  prescribe. Nunca en el hilo principal.
- **Sin dependencias Gradle nuevas** sin una razón en el PR. La app es
  pequeña a propósito (teléfonos económicos, 4 GB de RAM, datos
  intermitentes).
- **El listener no puede crashear jamás.** Todo en
  `AgenteNotificationListener.process` va en `try/catch` por notificación;
  mantenlo así — un crash ahí le cuesta al negocio cada mensaje que venga
  después.
- Mantén el mapa de archivos del `README.md` al día cuando agregues una
  pantalla o una clase.

## Depurar

```bash
adb logcat -s AgenteCore:V AgenteListener:V AgenteServer:V AgenteSecureStore:V agente.update:V UnknownAppObserver:V DeviceAttestation:V
```

- El puerto del núcleo cambia en cada arranque; `AgenteCore.baseUrl()` es la
  verdad. Para tocarlo desde tu máquina: `curl` no existe en la mayoría de
  teléfonos; usa `adb forward tcp:8127 tcp:<puerto>` y llama desde el host
  con el `X-App-Key` que lees de los logs (solo builds de debug).
- **Ajustes → mantén presionado el encabezado «Motor de IA»** (builds de
  debug) para apuntar el núcleo a tu propio endpoint compatible con OpenAI,
  p. ej. un modelo local vía `adb reverse`.
- El registro de actividad (Ajustes → Registro de actividad) muestra cada
  notificación que el listener manejó y por qué respondió o no.
- Emulador: no hay WhatsApp, pero cualquier app que publique una notificación
  `MessagingStyle` con acción de respuesta será contestada; el chat del
  gerente en el panel habla con el agente directamente.

## Lo que vive en otra parte

El núcleo del agente (Rust), la pasarela y la consola web son bases de código
separadas y privadas. Para cambios que necesiten un endpoint o una
herramienta nueva en el núcleo: abre un issue describiendo el comportamiento
que necesitas desde el punto de vista de `docs/CORE-API.md`.
