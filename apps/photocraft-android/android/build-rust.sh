#!/usr/bin/env bash
# Build all ABIs that Android Studio packages. Kept here instead of a Cargo plugin so a fresh
# checkout only needs the Android SDK/NDK and the pinned Rust toolchain.
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
workspace="$(cd -- "$script_dir/../../.." && pwd)"
out_dir=""

while (($#)); do
    case "$1" in
        --out)
            out_dir="${2:-}"
            shift 2
            ;;
        *)
            printf 'Unknown build-rust.sh argument: %s\n' "$1" >&2
            exit 2
            ;;
    esac
done

if [[ -z "$out_dir" ]]; then
    printf '%s\n' 'build-rust.sh needs --out <directory>' >&2
    exit 2
fi
: "${ANDROID_NDK_ROOT:?Set ANDROID_NDK_ROOT to an installed Android NDK.}"
# Android Studio's debug APK remains debuggable through logcat, while Rust line tables inflate the
# two packaged ABIs by hundreds of megabytes. Native symbols stay available from a separate local
# Cargo build when they are needed.
: "${CARGO_PROFILE_DEV_DEBUG:=0}"
export CARGO_PROFILE_DEV_DEBUG

case "$(uname -s)-$(uname -m)" in
    Linux-x86_64) host_tag="linux-x86_64" ;;
    Darwin-arm64) host_tag="darwin-arm64" ;;
    Darwin-x86_64) host_tag="darwin-x86_64" ;;
    *)
        printf 'Unsupported Android build host: %s-%s\n' "$(uname -s)" "$(uname -m)" >&2
        exit 2
        ;;
esac

toolchain="$ANDROID_NDK_ROOT/toolchains/llvm/prebuilt/$host_tag/bin"
if [[ ! -x "$toolchain/aarch64-linux-android26-clang" ]]; then
    printf 'Android NDK toolchain not found at %s\n' "$toolchain" >&2
    exit 2
fi

build_abi() {
    local target="$1"
    local abi="$2"
    local clang_prefix="$3"
    local env_key="CC_${target//-/_}"
    local ar_key="AR_${target//-/_}"
    local linker_key="CARGO_TARGET_${target//-/_}_LINKER"
    linker_key="${linker_key^^}"
    env "$env_key=$toolchain/${clang_prefix}26-clang" "$ar_key=$toolchain/llvm-ar" "$linker_key=$toolchain/${clang_prefix}26-clang" \
        cargo +1.95.0 build --manifest-path "$workspace/Cargo.toml" --package photocraft-android --target "$target"
    install -Dm755 "$workspace/target/$target/debug/libphotocraft_android.so" "$out_dir/$abi/libphotocraft_android.so"
}

build_abi aarch64-linux-android arm64-v8a aarch64-linux-android
build_abi x86_64-linux-android x86_64 x86_64-linux-android
