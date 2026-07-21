plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
    id("org.jetbrains.kotlin.plugin.compose")
}

android {
    namespace = "com.bindfetto.control"
    compileSdk = 36

    defaultConfig {
        applicationId = "com.bindfetto.control"
        minSdk = 26
        targetSdk = 36
        versionCode = 2
        versionName = "0.2.0"
    }

    buildTypes {
        release {
            isMinifyEnabled = false
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    kotlinOptions {
        jvmTarget = "17"
    }
    buildFeatures {
        compose = true
        buildConfig = true
    }

    // Extract jniLibs to nativeLibraryDir at install so the bundled bindfetto binary is a
    // real, executable file on disk (needed for the Deploy tab's launch attempt).
    packaging {
        jniLibs {
            useLegacyPackaging = true
        }
    }
}

// Bundle the cross-compiled runtime binary as an executable native lib. jniLibs is the one
// app location Android will place with exec permission. Skipped (with the Deploy tab
// showing its adb fallback) if the runtime hasn't been built yet.
val bundleRuntimeBinary by tasks.registering(Copy::class) {
    val src = rootProject.file("../runtime/target/aarch64-linux-android/release/bindfetto")
    onlyIf { src.exists() }
    from(src) { rename { "libbindfetto.so" } }
    into(layout.projectDirectory.dir("src/main/jniLibs/arm64-v8a"))
}
tasks.named("preBuild") { dependsOn(bundleRuntimeBinary) }

dependencies {
    implementation("androidx.core:core-ktx:1.13.1")
    implementation("androidx.activity:activity-compose:1.9.2")
    implementation("androidx.lifecycle:lifecycle-runtime-ktx:2.8.6")
    implementation("androidx.lifecycle:lifecycle-viewmodel-compose:2.8.6")
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-android:1.8.1")

    val composeBom = platform("androidx.compose:compose-bom:2024.09.03")
    implementation(composeBom)
    implementation("androidx.compose.ui:ui")
    implementation("androidx.compose.ui:ui-tooling-preview")
    implementation("androidx.compose.material3:material3")
    debugImplementation("androidx.compose.ui:ui-tooling")
}
