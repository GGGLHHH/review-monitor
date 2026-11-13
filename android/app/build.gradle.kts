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
        versionCode = 2
        versionName = "0.1.1"
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

// 登录辅助脚本（只注入美团登录表单所在的帧）：构建时从仓库根的 web/login.js 复制。
// 取数不再经过网页，以前的 poller.js 已经删掉。
val copyLoginJs by tasks.registering(Copy::class) {
    from(rootProject.file("../web/login.js"))
    into(layout.projectDirectory.dir("src/main/assets"))
}
tasks.named("preBuild") { dependsOn(copyLoginJs) }

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
