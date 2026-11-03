#!/usr/bin/env bash
set -euo pipefail
mkdir -p "$ANDROID_HOME/cmdline-tools"
if [[ ! -x "$ANDROID_HOME/cmdline-tools/20.0/bin/sdkmanager" ]]; then
    archive="$ANDROID_HOME/commandlinetools.zip"
    curl --fail --silent --show-error --location https://dl.google.com/android/repository/commandlinetools-linux-14742923_latest.zip -o "$archive"
    echo "04453066b540409d975c676d781da1477479dde3761310f1a7eb92a1dfb15af7  $archive" | sha256sum --check
    unzip -q "$archive" -d "$ANDROID_HOME/cmdline-tools"
    mv "$ANDROID_HOME/cmdline-tools/cmdline-tools" "$ANDROID_HOME/cmdline-tools/20.0"
    rm "$archive"
fi
sdkmanager="$ANDROID_HOME/cmdline-tools/20.0/bin/sdkmanager"
# Feed enough answers without a SIGPIPE from yes under pipefail.
"$sdkmanager" --sdk_root="$ANDROID_HOME" --licenses < <(printf 'y\n%.0s' {1..100})
"$sdkmanager" --sdk_root="$ANDROID_HOME" 'platform-tools' 'platforms;android-37.2' 'build-tools;37.0.0'
