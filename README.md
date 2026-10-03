# agento

An AI receptionist for small businesses that lives on the owner's Android
phone. It reads the notifications the phone already gets (WhatsApp and
WhatsApp Business, Instagram, Messenger, and Yape/Plin payment notices),
answers customers inline, books appointments, confirms payments and keeps a
CRM. It needs no platform APIs.

The app ships on Google Play as `yaya.tech.agento`.

## What's here

| path | what | language |
|---|---|---|
| `android/` | The app: notification listener, inline replies, onboarding, dashboard | Kotlin |
| `server/` | The agent core (`agente-core`). It runs **inside the app** as a native library, on SQLite | Rust |
| `gateway/` | `llm.yaya.tech`: Yaya ID accounts, the metered model proxy, encrypted backups, the relay, and the owner's web console at `/app` | Rust |
| `wire/` | What the core and the gateway share: agent identities and signed requests | Rust |
| `corazon/` | The plugin kernel the core is built on | Rust |
| `wa-otp/` | Sends the WhatsApp one-time codes for Yaya ID sign-in | Rust |

**Cloud sync:** every signed-in phone backs itself up, encrypted, to its Yaya
account (at most every six hours, after activity). The owner sees and steers
the phone's agent from the web console through the gateway's sealed relay;
the gateway carries ciphertext only.

## Build

```bash
# Core: unit tests (each gets its own SQLite file; fake upstreams; no network)
cd server && cargo test

# Gateway
cd gateway && cargo test

# Android: build the core for the phone first (the .so is not committed)
cd server && cargo ndk -t arm64-v8a -t armeabi-v7a -P 26 \
    -o ../android/app/src/main/jniLibs build --release --lib
cd ../android && echo "sdk.dir=$ANDROID_HOME" > local.properties
./gradlew testDebugUnitTest assembleDebug
```

Requirements: stable Rust, `cargo-ndk`, JDK 17+, Android SDK 36 with NDK
`27.2.12479018`. Releases: `android/docs/RELEASE.md`; Play submission:
`android/docs/PLAY.md`.

## Licenses

`android/` is AGPL-3.0-or-later. `wire/` and `corazon/` are MIT or
Apache-2.0. `server/`, `gateway/` and `wa-otp/` do not carry a license yet.
