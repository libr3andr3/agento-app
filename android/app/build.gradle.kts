// SPDX-License-Identifier: AGPL-3.0-or-later
plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

android {
    // The Kotlin package is `tech.yaya.agente` (the app's original name).
    // It cannot change: the agent core's JNI entry points are resolved by
    // this name (Java_tech_yaya_agente_AgenteCore_*), see AgenteCore.kt.
    namespace = "tech.yaya.agente"
    compileSdk = 36

    defaultConfig {
        // The one app: the package registered in Play Console.
        applicationId = "yaya.tech.agento"
        minSdk = 26
        targetSdk = 36
        versionCode = 78
        versionName = "1.30.0"
        // ARM phones only: the agent core is built for arm64-v8a and
        // armeabi-v7a (jniLibs/CORE.md). Without the filter the APK would
        // still carry x86 builds of the other native libraries (WireGuard)
        // and install on an x86 device that then cannot load the core.
        ndk { abiFilters += listOf("arm64-v8a", "armeabi-v7a") }

        resValue("string", "app_name", "agento")
        // Where accounts and plans are managed (the gateway's web app).
        val webApp = System.getenv("AGENTE_WEB_APP") ?: "https://agente.ceo/app"
        buildConfigField("String", "WEB_APP_URL", "\"$webApp\"")
        // D18: nothing is sold inside the app. The plan screen is gone; money
        // changes hands only at agente.ceo/checkout, which the web account
        // links to. Kept for the core's sales contact; the app never opens it.
        val sales = System.getenv("AGENTE_SALES_PHONE") ?: "51913879819"
        buildConfigField("String", "SALES_WHATSAPP", "\"$sales\"")
        // The support line the app opens until the server tells it otherwise
        // (Prefs.supportPhone caches what /api/plan → support.phone says).
        val support = System.getenv("AGENTE_SUPPORT_PHONE") ?: "51952183367"
        buildConfigField("String", "SUPPORT_WHATSAPP", "\"$support\"")
        // The gateway the house phone's till forwards Yape notifications to
        // (YapeCollector). Compiled in so a link can never redirect them.
        val gateway = System.getenv("AGENTE_GATEWAY_URL") ?: "https://llm.yaya.tech"
        buildConfigField("String", "GATEWAY_URL", "\"$gateway\"")
    }

    buildFeatures {
        buildConfig = true
    }

    // The app ships in Spanish (res/values), Portuguese and English; library
    // resources in any other language are dropped from the APK.
    androidResources {
        localeFilters += listOf("es", "pt", "en")
    }

    // Release signing. The upload key lives OUTSIDE the repo; export
    // AGENTE_KEYSTORE / AGENTE_KEYSTORE_PASS / AGENTE_KEY_ALIAS / AGENTE_KEY_PASS
    // (docs/RELEASE.md). Without them a release build is unsigned but still
    // compiles, so CI and contributors are never blocked.
    signingConfigs {
        create("upload") {
            val ks = System.getenv("AGENTE_KEYSTORE")
            if (ks != null) {
                storeFile = file(ks)
                storePassword = System.getenv("AGENTE_KEYSTORE_PASS")
                keyAlias = System.getenv("AGENTE_KEY_ALIAS") ?: "agente-upload"
                keyPassword = System.getenv("AGENTE_KEY_PASS")
            }
        }
    }

    buildTypes {
        release {
            if (System.getenv("AGENTE_KEYSTORE") != null) {
                signingConfig = signingConfigs.getByName("upload")
            }
            isMinifyEnabled = true
            isShrinkResources = true
            proguardFiles(
                getDefaultProguardFile("proguard-android-optimize.txt"),
                "proguard-rules.pro"
            )
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    kotlinOptions {
        jvmTarget = "17"
    }

    // JVM unit tests (src/test): plain JUnit for pure logic, Robolectric for
    // anything that needs a Context, SharedPreferences or SQLite.
    testOptions {
        unitTests {
            isIncludeAndroidResources = true
        }
    }
}

// The agent core (libagente_core.so, one per ABI) is a prebuilt native
// library checked in under src/main/jniLibs — see docs/ARCHITECTURE.md and
// src/main/jniLibs/CORE.md. Its schemas ship as assets (src/main/assets/schemas).
// Nothing here needs Rust, the NDK or a network connection to build.

dependencies {
    implementation("androidx.core:core-ktx:1.15.0")
    implementation("androidx.appcompat:appcompat:1.7.0")
    implementation("com.google.android.material:material:1.12.0")
    // EXIF orientation fix for catalog photos before upload.
    implementation("androidx.exifinterface:exifinterface:1.3.7")
    implementation("androidx.recyclerview:recyclerview:1.3.2")
    implementation("androidx.swiperefreshlayout:swiperefreshlayout:1.1.0")
    // Card top-ups open the Dodo checkout in a Custom Tab (CreditsActivity).
    implementation("androidx.browser:browser:1.8.0")
    // Play Store install-referrer attribution (utm_source/medium/campaign
    // captured at install time).
    implementation("com.android.installreferrer:installreferrer:2.2")
    // yaya mesh: userspace WireGuard through VpnService (the app's p2p VPN).
    implementation("com.wireguard.android:tunnel:1.0.20260102")

    testImplementation("junit:junit:4.13.2")
    testImplementation("org.robolectric:robolectric:4.14.1")
    testImplementation("androidx.test:core:1.6.1")
    // The real org.json, so pure-JVM tests are not reading android.jar stubs.
    testImplementation("org.json:json:20240303")
}
