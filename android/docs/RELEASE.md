# Publicar una versión

La app se publica solo en Google Play, como `yaya.tech.agento`. Play entrega
las actualizaciones; no hay otro canal.

## Versiones

`app/build.gradle.kts` → `versionCode` (entero, siempre sube) y `versionName`
(`major.minor.patch`). Play rechaza un `versionCode` que ya subiste.

## El núcleo del agente

`app/src/main/jniLibs/<abi>/libagente_core.so` no está en el repo: se compila
desde `../server` para las dos ABIs ARM antes de cada release.

```bash
cd ../server
cargo ndk -t arm64-v8a -t armeabi-v7a -P 26 -o ../android/app/src/main/jniLibs build --release --lib
```

`AgenteCore.installSchemas` re-copia `assets/schemas/` cada vez que cambia el
`versionCode`; si los esquemas del núcleo cambiaron, cópialos también.

## Build firmado

El keystore de subida nunca entra al repo. Carga sus variables (con `set -a`,
o Gradle produce artefactos sin firmar sin avisar):

```bash
set -a; . ~/.agente/keystore.env; set +a   # AGENTE_KEYSTORE / _PASS / _ALIAS / KEY_PASS
./gradlew bundleRelease assembleRelease
# app/build/outputs/bundle/release/app-release.aab   ← esto se sube a Play
# app/build/outputs/apk/release/app-release.apk      ← adb install para una pasada de cordura
jarsigner -verify app/build/outputs/bundle/release/app-release.aab
$ANDROID_HOME/build-tools/<ver>/apksigner verify --print-certs app/build/outputs/apk/release/app-release.apk
# Signer #1 certificate SHA-256 digest: 0204f2e455438244720aa79c9421e70927d957259cc77ce95b69148a44a35df2
```

Nunca compiles con `AGENTE_RELAY_APP_KEY`: ya no existe en el build, y una
versión vieja que lo usaba rompía el registro con un 403.

## Checklist

- [ ] `versionCode` subido; `versionName` coincide con lo que vas a anunciar
- [ ] núcleo recompilado desde `../server` para arm64-v8a y armeabi-v7a
- [ ] `./gradlew testDebugUnitTest lintDebug` en verde
- [ ] `.aab` firmado por la clave de subida (digest de arriba)
- [ ] instalado en un teléfono real: iniciar sesión, registrar, la
      entrevista, un mensaje de WhatsApp respondido, una notificación de
      Yape registrada, un respaldo en la cuenta
- [ ] subido a Play Console con las notas de `CHANGELOG.md` (ver `docs/PLAY.md`)
