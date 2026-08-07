#!/usr/bin/env bash

set -euo pipefail

if [ "$#" -ne 2 ]; then
    echo "usage: $0 <native-macos|macosarm64|macosx64> <destination>" >&2
    exit 2
fi

platform="$1"
destination="$2"

if [ "$platform" = "native-macos" ]; then
    case "$(uname -m)" in
        arm64) platform="macosarm64" ;;
        x86_64) platform="macosx64" ;;
        *)
            echo "unsupported macOS architecture: $(uname -m)" >&2
            exit 1
            ;;
    esac
fi

case "$platform" in
    macosarm64 | macosx64) ;;
    *)
        echo "unsupported CEF platform: $platform" >&2
        exit 1
        ;;
esac

script_dir="$(cd "$(dirname "$0")" && pwd)"
repo_root="$(cd "$script_dir/.." && pwd)"
version="$(tr -d '\r\n' < "$repo_root/cef-version.txt")"
archive="$destination/cef_binary_${version}_${platform}.tar.bz2"
extract_dir="$destination/extracted"
cef_root="$extract_dir/cef_binary_${version}_${platform}"

mkdir -p "$destination" "$extract_dir"
curl --fail --location --silent --show-error \
    "https://cef-builds.spotifycdn.com/cef_binary_${version}_${platform}.tar.bz2" \
    --output "$archive"
tar -xjf "$archive" -C "$extract_dir"

if [ ! -d "$cef_root" ]; then
    echo "CEF archive did not contain the expected directory: $cef_root" >&2
    exit 1
fi

printf '%s\n' "$cef_root"
