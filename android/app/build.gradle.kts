plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.plugin.compose")
}

android {
    namespace = "com.reviewmonitor"
    compileSdk = 37
    buildToolsVersion = "36.1.0"

    defaultConfig {
        applicationId = "com.reviewmonitor"
        minSdk = 28
        targetSdk = 36
        versionCode = 1
        versionName = "0.1.0"
        ndk { abiFilters += listOf("arm64-v8a", "x86_64") }
    }

    buildTypes {
        release {
            isMinifyEnabled = false
            signingConfig = signingConfigs.getByName("debug")
        }
    }
    buildFeatures { compose = true }
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    // Rust 编译出来的 .so（cargo-ndk 输出到这里）
    sourceSets["main"].jniLibs.directories.add("src/main/jniLibs")
}

// 注入页面的脚本和电脑版共用一份：构建时从仓库根的 web/poller.js 复制
val copyPollerJs by tasks.registering(Copy::class) {
    from(rootProject.file("../web/poller.js"), rootProject.file("../web/login.js"))
    into(layout.projectDirectory.dir("src/main/assets"))
}
tasks.named("preBuild") { dependsOn(copyPollerJs) }

dependencies {
    val composeBom = platform("androidx.compose:compose-bom:2026.09.00")
    implementation(composeBom)
    implementation("androidx.compose.material3:material3")
    implementation("androidx.compose.ui:ui")
    implementation("androidx.activity:activity-compose:1.13.0")
    implementation("androidx.core:core-ktx:1.19.1")
    implementation("androidx.webkit:webkit:1.18.0")
    implementation("androidx.lifecycle:lifecycle-runtime-compose:2.11.0")
    implementation("net.java.dev.jna:jna:5.19.1@aar")
}
