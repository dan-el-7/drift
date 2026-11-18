import java.util.Properties

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.plugin.compose")
}

android {
    namespace = "app.drift"
    compileSdk = 37
    defaultConfig {
        applicationId = "app.drift"
        minSdk = 31 // RenderEffect (Ink's blur + threshold)
        targetSdk = 37
        versionCode = 8
        versionName = "0.2.7"
        ndk { abiFilters += "arm64-v8a" }
    }
    buildTypes {
        release {
            isMinifyEnabled = true
            proguardFiles(getDefaultProguardFile("proguard-android-optimize.txt"), "proguard-rules.pro")
            signingConfig = signingConfigs.getByName("debug") // real signing later
        }
    }
    buildFeatures { compose = true }
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
}

dependencies {
    val bom = platform("androidx.compose:compose-bom:2026.09.00")
    implementation(bom)
    implementation("androidx.compose.ui:ui")
    implementation("androidx.compose.foundation:foundation")
    implementation("androidx.compose.animation:animation")
    implementation("androidx.activity:activity-compose:1.13.0")
    implementation("androidx.core:core-ktx:1.19.1")
}

// The Rust core (drift_ffi, the same core the Windows app uses), built for the phone.
val cargoNdk = tasks.register<Exec>("cargoNdk") {
    val repo = rootProject.projectDir.parentFile
    workingDir = repo
    val sdk = Properties().apply { rootProject.file("local.properties").inputStream().use { load(it) } }.getProperty("sdk.dir")
    val ndk = File(sdk, "ndk").listFiles()?.filter { it.isDirectory }?.maxByOrNull { it.name }
    if (ndk != null) environment("ANDROID_NDK_HOME", ndk.absolutePath)
    // The NDK's clang wrapper can't take paths with an apostrophe: build elsewhere.
    environment("CARGO_TARGET_DIR", File(System.getenv("LOCALAPPDATA") ?: System.getProperty("java.io.tmpdir"), "drift-build/android-target").absolutePath)
    // Rust embeds source paths (panic messages): map this machine's folders away. Set here, the
    // config's target rustflags are ignored, so aes_armv8 (.cargo/config.toml) is repeated.
    val home = System.getProperty("user.home")
    environment("CARGO_ENCODED_RUSTFLAGS", listOf("--cfg", "aes_armv8", "--remap-path-prefix=$home=~", "--remap-path-prefix=${repo.absolutePath}=.").joinToString("\u001f"))
    val release = gradle.startParameter.taskNames.any { it.contains("Release", ignoreCase = true) }
    commandLine(
        listOf("cargo", "ndk", "-t", "arm64-v8a", "-P", "31", "-o", "android/app/src/main/jniLibs", "build", "-p", "drift-ffi")
            + if (release) listOf("--release") else listOf("--profile", "android-dev")
    )
}
tasks.named("preBuild") { dependsOn(cargoNdk) }
