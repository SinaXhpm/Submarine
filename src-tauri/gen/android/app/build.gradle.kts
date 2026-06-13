import java.util.Properties

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
    id("rust")
}

val tauriProperties = Properties().apply {
    val propFile = file("tauri.properties")
    if (propFile.exists()) {
        propFile.inputStream().use { load(it) }
    }
}

// Release-signing config — resolved in this order:
//   1. keystore.properties next to this build.gradle (local dev)
//   2. SUBMARINE_KEYSTORE_* env vars (CI: GitHub Actions secrets)
// If neither is present, release builds fall back to unsigned. The
// keystore file itself is .gitignored and lives outside the repo.
val keystorePropertiesFile = rootProject.file("keystore.properties")
val keystoreProperties = Properties().apply {
    if (keystorePropertiesFile.exists()) {
        keystorePropertiesFile.inputStream().use { load(it) }
    }
}
val resolvedKeystorePath: String? =
    keystoreProperties.getProperty("storeFile")
        ?: System.getenv("SUBMARINE_KEYSTORE_PATH")
val resolvedStorePassword: String? =
    keystoreProperties.getProperty("storePassword")
        ?: System.getenv("SUBMARINE_KEYSTORE_PASSWORD")
val resolvedKeyAlias: String? =
    keystoreProperties.getProperty("keyAlias")
        ?: System.getenv("SUBMARINE_KEY_ALIAS")
val resolvedKeyPassword: String? =
    keystoreProperties.getProperty("keyPassword")
        ?: System.getenv("SUBMARINE_KEY_PASSWORD")
val hasReleaseSigning: Boolean =
    resolvedKeystorePath != null && resolvedStorePassword != null
        && resolvedKeyAlias != null && resolvedKeyPassword != null
        && file(resolvedKeystorePath!!).exists()

android {
    compileSdk = 36
    namespace = "com.submarine.app"
    defaultConfig {
        manifestPlaceholders["usesCleartextTraffic"] = "false"
        applicationId = "com.submarine.app"
        minSdk = 24
        targetSdk = 36
        versionCode = tauriProperties.getProperty("tauri.android.versionCode", "1").toInt()
        versionName = tauriProperties.getProperty("tauri.android.versionName", "1.0")
    }
    signingConfigs {
        if (hasReleaseSigning) {
            create("release") {
                storeFile = file(resolvedKeystorePath!!)
                storePassword = resolvedStorePassword
                keyAlias = resolvedKeyAlias
                keyPassword = resolvedKeyPassword
                // Sign with v1 + v2 + v3 so the APK installs on every minSdk
                // 24+ device. Some Android variants reject v2-only APKs at
                // install time ("Package appears to be invalid"); shipping
                // the full triad removes that ambiguity. v4 is intentionally
                // off — it needs a side-band .apk.idsig file Play uses for
                // incremental install but standalone sideload doesn't.
                enableV1Signing = true
                enableV2Signing = true
                enableV3Signing = true
                enableV4Signing = false
            }
        }
    }
    buildTypes {
        getByName("debug") {
            manifestPlaceholders["usesCleartextTraffic"] = "true"
            isDebuggable = true
            isJniDebuggable = true
            isMinifyEnabled = false
            packaging {                jniLibs.keepDebugSymbols.add("*/arm64-v8a/*.so")
                jniLibs.keepDebugSymbols.add("*/armeabi-v7a/*.so")
                jniLibs.keepDebugSymbols.add("*/x86/*.so")
                jniLibs.keepDebugSymbols.add("*/x86_64/*.so")
            }
        }
        getByName("release") {
            isMinifyEnabled = true
            proguardFiles(
                *fileTree(".") { include("**/*.pro") }
                    .plus(getDefaultProguardFile("proguard-android-optimize.txt"))
                    .toList().toTypedArray()
            )
            if (hasReleaseSigning) {
                signingConfig = signingConfigs.getByName("release")
            }
        }
    }
    kotlinOptions {
        jvmTarget = "1.8"
    }
    buildFeatures {
        buildConfig = true
    }
}

rust {
    rootDirRel = "../../../"
}

dependencies {
    implementation("androidx.webkit:webkit:1.14.0")
    implementation("androidx.appcompat:appcompat:1.7.1")
    implementation("androidx.activity:activity-ktx:1.10.1")
    implementation("com.google.android.material:material:1.12.0")
    implementation("androidx.lifecycle:lifecycle-process:2.10.0")
    testImplementation("junit:junit:4.13.2")
    androidTestImplementation("androidx.test.ext:junit:1.1.4")
    androidTestImplementation("androidx.test.espresso:espresso-core:3.5.0")
}

apply(from = "tauri.build.gradle.kts")