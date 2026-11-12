#!/usr/bin/env bash
# 编译 Rust 核心（rm-ffi）给安卓用，并重新生成 Kotlin 绑定。改了 crates/ 下的代码后先跑它，再用 Gradle 打包。
# 需要：rustup target add aarch64-linux-android x86_64-linux-android；cargo install cargo-ndk；NDK（ANDROID_NDK_HOME 或 SDK 下的 ndk/）
set -euo pipefail
cd "$(dirname "$0")/.."
SDK="${ANDROID_HOME:-${LOCALAPPDATA:-$HOME}/Android/Sdk}"
export ANDROID_NDK_HOME="${ANDROID_NDK_HOME:-$(ls -d "$SDK"/ndk/* | sort -V | tail -1)}"
cargo ndk -t arm64-v8a -t x86_64 -o android/app/src/main/jniLibs build -p rm-ffi --release
cargo build -p rm-ffi
lib=""
for f in target/debug/rm_ffi.dll target/debug/librm_ffi.so target/debug/librm_ffi.dylib; do [ -f "$f" ] && lib="$f" && break; done
cargo run -q -p rm-ffi --features cli --bin uniffi-bindgen -- \
  generate --library "$lib" --language kotlin --out-dir android/app/src/main/java --no-format
echo "OK: jniLibs 和 com/reviewmonitor/core/rm_ffi.kt 已更新"
