#!/bin/sh
# Linux counterpart of install-toolchain.ps1: the official Rust parts for x86_64-unknown-linux-gnu (rustc, cargo,
# rust-std) into DESTINATION, each checked against the SHA-256 static.rust-lang.org publishes. No rustup, no PATH or
# profile edits. The game's build also needs a few system packages (a C compiler, pkg-config, ALSA and udev
# headers); they are checked first so a missing one is reported before several hundred MB are downloaded.
# Usage: install-toolchain.sh DESTINATION VERSION
set -eu
DEST=$1
VERSION=$2
TARGET=x86_64-unknown-linux-gnu

missing=""
command -v cc >/dev/null 2>&1 || missing="$missing C-compiler"
if command -v pkg-config >/dev/null 2>&1; then
    pkg-config --exists alsa || missing="$missing ALSA-headers"
    pkg-config --exists libudev || missing="$missing udev-headers"
else
    missing="$missing pkg-config"
fi
for tool in curl tar xz sha256sum; do
    command -v "$tool" >/dev/null 2>&1 || missing="$missing $tool"
done
if [ -n "$missing" ]; then
    echo "Missing system packages:$missing"
    if command -v apt-get >/dev/null 2>&1; then
        echo "Install them with:  sudo apt install build-essential pkg-config libasound2-dev libudev-dev curl xz-utils"
    elif command -v dnf >/dev/null 2>&1; then
        echo "Install them with:  sudo dnf install gcc pkgconf-pkg-config alsa-lib-devel systemd-devel curl xz"
    elif command -v pacman >/dev/null 2>&1; then
        echo "Install them with:  sudo pacman -S --needed base-devel alsa-lib systemd-libs curl xz"
    elif command -v zypper >/dev/null 2>&1; then
        echo "Install them with:  sudo zypper install gcc pkg-config alsa-devel libudev-devel curl xz"
    else
        echo "Install a C compiler, pkg-config and the ALSA and libudev development packages, then retry."
    fi
    echo "Then press Install / Retry again."
    exit 3
fi

downloads="$DEST/downloads"
mkdir -p "$downloads"
rm -f "$DEST/complete"
for component in rustc cargo rust-std; do
    name="$component-$VERSION-$TARGET"
    archive="$downloads/$name.tar.xz"
    url="https://static.rust-lang.org/dist/$name.tar.xz"
    expected=$(curl --fail --silent --show-error --location --retry 3 "$url.sha256" | cut -d' ' -f1)
    case "$expected" in
        [0-9a-f]*) [ ${#expected} -eq 64 ] || { echo "Invalid SHA-256 for $name"; exit 1; } ;;
        *) echo "Could not read the SHA-256 for $name. Check your internet connection and retry."; exit 1 ;;
    esac
    if [ ! -f "$archive" ] || [ "$(sha256sum "$archive" | cut -d' ' -f1)" != "$expected" ]; then
        echo "Downloading $component $VERSION. Progress below: transferred bytes / total bytes. Retry resumes partial files."
        if ! curl --fail --location --retry 3 --connect-timeout 30 --continue-at - --output "$archive" "$url"; then
            echo "Resume failed; retrying a fresh download (the server may not support this partial range)."
            curl --fail --location --retry 3 --connect-timeout 30 --output "$archive" "$url" ||
                { echo "Download failed. Check your internet connection and retry to resume $component."; exit 1; }
        fi
        if [ "$(sha256sum "$archive" | cut -d' ' -f1)" != "$expected" ]; then
            rm -f "$archive"
            echo "SHA-256 mismatch for $component. Corrupt archive removed; retry to download again."
            exit 1
        fi
    fi
    echo "Verified SHA-256: $component $VERSION ($(wc -c < "$archive") bytes). Extracting..."
    rm -rf "$downloads/$name"
    tar -xJf "$archive" -C "$downloads"
    sh "$downloads/$name/install.sh" --prefix="$DEST" --disable-ldconfig --without=rust-docs >/dev/null
    rm -rf "$downloads/$name"
done
"$DEST/bin/rustc" --version
"$DEST/bin/cargo" --version
echo "$VERSION linux" > "$DEST/complete"
echo "Toolchain complete."
