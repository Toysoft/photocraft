import java.util.Properties

plugins {
    id("com.android.application")
}

val localProperties = Properties().apply {
    val local = rootProject.file("local.properties")
    if (local.isFile) {
        local.inputStream().use(::load)
    }
}
val sdkDirectory = providers.environmentVariable("ANDROID_SDK_ROOT").orNull
    ?: providers.environmentVariable("ANDROID_HOME").orNull
    ?: localProperties.getProperty("sdk.dir")
    ?: error("Set ANDROID_SDK_ROOT or sdk.dir in android/local.properties.")
val ndkDirectory = providers.environmentVariable("ANDROID_NDK_ROOT").orNull
    ?: providers.environmentVariable("ANDROID_NDK_HOME").orNull
    ?: File(sdkDirectory, "ndk/30.0.16248370").absolutePath
val rustJniLibs = layout.buildDirectory.dir("rust-jniLibs")

android {
    namespace = "ai.storyteller.photocraft"
    compileSdk = 36

    defaultConfig {
        applicationId = "ai.storyteller.photocraft"
        minSdk = 26
        targetSdk = 36
        versionCode = 1
        versionName = "0.8.2"
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    sourceSets {
        getByName("main").jniLibs.srcDir(rustJniLibs)
    }
}

val buildRust = tasks.register<Exec>("buildRust") {
    workingDir = rootProject.projectDir
    commandLine("bash", "build-rust.sh", "--out", rustJniLibs.get().asFile.absolutePath)
    environment("ANDROID_SDK_ROOT", sdkDirectory)
    environment("ANDROID_NDK_ROOT", ndkDirectory)
    environment("CARGO_PROFILE_DEV_DEBUG", "0")
    inputs.file(rootProject.file("build-rust.sh"))
    inputs.files(
        fileTree(rootProject.projectDir.parentFile.parentFile.parentFile) {
            include("Cargo.toml", "Cargo.lock", "apps/photocraft-android/Cargo.toml", "apps/photocraft-android/src/**", "crates/**")
            exclude("**/target/**")
        },
    )
    outputs.dir(rustJniLibs)
}

tasks.configureEach {
    if (name.endsWith("JniLibFolders") || name.endsWith("NativeLibs")) {
        dependsOn(buildRust)
    }
}
