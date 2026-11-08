#!/usr/bin/env bash
# Builds a signed debug APK. Needs ANDROID_HOME, ANDROID_NDK_HOME, cargo-ndk and a JDK.
# Usage: build-apk.sh OUTPUT.apk [TARGET_DIR]
set -euo pipefail

output=$(realpath -m "$1")
cd "$(dirname "$0")/../.."
target=${2:-target}
work=$(realpath -m "$target/android-apk")
platform=$(ls -d "$ANDROID_HOME"/platforms/android-* | sort -V | tail -1)
tools=$(ls -d "$ANDROID_HOME"/build-tools/* | sort -V | tail -1)

cargo ndk -t arm64-v8a -P 26 build --release --locked -p cantus-app --lib

rm -rf "$work"
mkdir -p "$work/res/mipmap-xxxhdpi" "$work/lib/arm64-v8a" "$(dirname "$output")"
cp packaging/icon.png "$work/res/mipmap-xxxhdpi/icon.png"
cp "$target/aarch64-linux-android/release/libcantus_app.so" "$work/lib/arm64-v8a/"

"$tools/aapt2" compile --dir "$work/res" -o "$work/res.zip"
"$tools/aapt2" link -o "$work/base.apk" -I "$platform/android.jar" \
    --manifest packaging/android/AndroidManifest.xml \
    --min-sdk-version 26 --target-sdk-version 36 "$work/res.zip"
(cd "$work" && "$tools/aapt" add base.apk lib/arm64-v8a/libcantus_app.so > /dev/null)
"$tools/zipalign" -f -p 4 "$work/base.apk" "$work/aligned.apk"

keystore=${CANTUS_KEYSTORE:-$(dirname "$work")/android-debug.keystore}
if [ ! -f "$keystore" ]; then
    keytool -genkeypair -keystore "$keystore" -storepass android -keypass android \
        -alias debug -keyalg RSA -keysize 2048 -validity 10000 -dname "CN=Cantus Debug"
fi
apksigner="$tools/apksigner"
[ -f "$apksigner" ] || apksigner="$apksigner.bat"
"$apksigner" sign --ks "$keystore" --ks-pass pass:android --out "$output" "$work/aligned.apk"
