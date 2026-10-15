#!/usr/bin/env bash
# Build and package PhotoCraft for Linux (<arch> is x86_64, aarch64 or riscv64):
#
#   $DIST/photocraft-<version>-linux-<arch>.AppImage  any distro with glibc >= the build host's
#   $DIST/photocraft-<version>-linux-<arch>.AppImage.zsync  delta updates (needs zsyncmake)
#   $DIST/photocraft-<version>-linux-<arch>.deb       Debian, Ubuntu, Mint, Pop!_OS, ...
#   $DIST/photocraft-<version>-linux-<arch>.rpm       Fedora, openSUSE, RHEL, ...
#   $DIST/photocraft-<version>-linux-<arch>.pkg.tar.zst  Arch, Manjaro, EndeavourOS, ...
#   $DIST/photocraft-<version>-linux-<arch>.tar.gz    plain FHS-style tree (bin/, share/)
#
# Usage: packaging/linux/package.sh [--skip-build] [--formats "appimage deb rpm arch tar"]
#
# Needs: cargo; nfpm for deb/rpm/arch (https://nfpm.goreleaser.com). By default AppImage tooling
# is downloaded at verified pins into $CARGO_TARGET_DIR. Build on an old distro (CI: Ubuntu 22.04,
# glibc 2.35) so the binaries run on newer ones. Optional: desktop-file-validate, appstreamcli,
# zsyncmake (the zsync package) for the AppImage's .zsync.
#
# Cross builds (CI: riscv64, tar.gz only): set CROSS_ARCH (riscv64), CROSS_TARGET (the Rust
# triple), CROSS_COMPILE (binutils prefix, for strip) and optionally EMULATOR (e.g. qemu-riscv64)
# to smoke-test the CLI.
set -euo pipefail
# shellcheck source=../env.sh
. "$(dirname "${BASH_SOURCE[0]}")/../env.sh"
HERE="$ROOT/packaging/linux"
# shellcheck source=verified-download.sh
. "$HERE/verified-download.sh"
APP_ID=ai.storyteller.photocraft

SKIP_BUILD=0
FORMATS="appimage deb rpm arch tar"
while [ $# -gt 0 ]; do
  case "$1" in
    --skip-build) SKIP_BUILD=1; shift ;;
    --formats) FORMATS="$2"; shift 2 ;;
    -h | --help) sed -n '2,20p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

ARCH="${CROSS_ARCH:-$(uname -m)}"
case "$ARCH" in
  riscv64) DEB_ARCH=riscv64 ;;
  x86_64) DEB_ARCH=amd64 ;;
  aarch64 | arm64) ARCH=aarch64; DEB_ARCH=arm64 ;;
  *) echo "unsupported architecture $ARCH" >&2; exit 2 ;;
esac
export PHOTOCRAFT_MAINTAINER="${PHOTOCRAFT_MAINTAINER:-PhotoCraft maintainers <photocraft@storyteller.ai>}"
BASENAME="photocraft-$VERSION-linux-$ARCH"

echo "==> PhotoCraft $VERSION for Linux $ARCH ($FORMATS)"

if [ "$SKIP_BUILD" = 0 ]; then
  if [ -n "${CROSS_TARGET:-}" ]; then
    (cd "$ROOT" && cargo build --release --locked -p photocraft -p photocraft-cli --features heif --target "$CROSS_TARGET")
  else
    (cd "$ROOT" && cargo build --release --locked -p photocraft -p photocraft-cli --features heif)
  fi
fi
BIN="$CARGO_TARGET_DIR/${CROSS_TARGET:+$CROSS_TARGET/}release"
WORK="$CARGO_TARGET_DIR/linux-package"
STAGE="$WORK/root"
rm -rf "$WORK"

# ---- stage an FHS tree (shared by every format) -------------------------------------------------
install -Dm755 "$BIN/photocraft" "$STAGE/usr/bin/photocraft"
install -Dm755 "$BIN/photocraft-cli" "$STAGE/usr/bin/photocraft-cli"
"${CROSS_COMPILE:-}strip" "$STAGE/usr/bin/photocraft" "$STAGE/usr/bin/photocraft-cli" 2>/dev/null || true
install -Dm644 "$HERE/$APP_ID.desktop" "$STAGE/usr/share/applications/$APP_ID.desktop"
install -Dm644 "$HERE/$APP_ID.mime.xml" "$STAGE/usr/share/mime/packages/$APP_ID.xml"
mkdir -p "$STAGE/usr/share/metainfo"
sed -e "s/@VERSION@/$VERSION/g" -e "s/@DATE@/$PHOTOCRAFT_BUILD_DATE/g" \
  "$HERE/$APP_ID.metainfo.xml.in" >"$STAGE/usr/share/metainfo/$APP_ID.metainfo.xml"
mkdir -p "$STAGE/usr/share/icons"
cp -R "$ROOT/assets/app-icon/hicolor" "$STAGE/usr/share/icons/"
mkdir -p "$STAGE/usr/share/doc/photocraft"
copy_docs "$STAGE/usr/share/doc/photocraft"

if command -v desktop-file-validate >/dev/null; then
  desktop-file-validate "$STAGE/usr/share/applications/$APP_ID.desktop"
fi
if command -v appstreamcli >/dev/null; then
  appstreamcli validate --no-net --explain "$STAGE/usr/share/metainfo/$APP_ID.metainfo.xml"
fi

has() { case " $FORMATS " in *" $1 "*) return 0 ;; *) return 1 ;; esac; }

# ---- .tar.gz ------------------------------------------------------------------------------------
if has tar; then
  mkdir -p "$WORK/tar"
  cp -R "$STAGE/usr" "$WORK/tar/$BASENAME"
  tar -C "$WORK/tar" -czf "$DIST/$BASENAME.tar.gz" "$BASENAME"
  echo "wrote $DIST/$BASENAME.tar.gz"
fi

# ---- .deb / .rpm / .pkg.tar.zst -----------------------------------------------------------------
if has deb || has rpm || has arch; then
  command -v nfpm >/dev/null || { echo "error: nfpm not found (https://nfpm.goreleaser.com/install/)" >&2; exit 1; }
  export VERSION
  export NFPM_ARCH="$DEB_ARCH"
  # nfpm expands env vars in fields like `version` and `arch`, but not in `contents[].src`.
  sed "s|\${STAGE}|$STAGE|g" "$HERE/nfpm.yaml" >"$WORK/nfpm.yaml"
  for fmt in deb rpm; do
    if has "$fmt"; then (cd "$ROOT" && nfpm package -f "$WORK/nfpm.yaml" -p "$fmt" -t "$DIST/$BASENAME.$fmt"); fi
  done
  # The Arch package is named by the bundle id (see `archlinux` in nfpm.yaml), which nfpm can't
  # override per format, and `archlinux.packager` isn't env-expanded, so both are written here.
  if has arch; then
    sed "s|^name: photocraft\$|name: $APP_ID|" "$WORK/nfpm.yaml" >"$WORK/nfpm-arch.yaml"
    grep -qx "name: $APP_ID" "$WORK/nfpm-arch.yaml" || { echo "error: couldn't rename the Arch package" >&2; exit 1; }
    printf 'archlinux:\n  packager: "%s"\n' "$PHOTOCRAFT_MAINTAINER" >>"$WORK/nfpm-arch.yaml"
    # nfpm (2.47, 2.48) writes Go's directory bit into the mode of each directory a `type: tree`
    # entry walks (0o20000000755), and pacman then warns "directory permissions differ" on every
    # shared directory under /usr/share. List the tree's files one by one instead: nfpm gives their
    # parent directories a plain 0755.
    if find "$STAGE/usr/share" ! -type d ! -type f | grep -q .; then
      echo "error: only plain files can be listed in the Arch package's usr/share" >&2; exit 1
    fi
    find "$STAGE/usr/share" -type f | LC_ALL=C sort | while IFS= read -r f; do
      printf '  - src: "%s"\n    dst: "%s"\n' "$f" "${f#"$STAGE"}"
    done >"$WORK/arch-share.yaml"
    awk -v files="$WORK/arch-share.yaml" -v tree="  - src: $STAGE/usr/share/" '
      $0 == tree { while ((getline line < files) > 0) print line; skip = 2; next }
      skip > 0 { skip--; next }
      { print }
    ' "$WORK/nfpm-arch.yaml" >"$WORK/nfpm-arch.yaml.new"
    mv "$WORK/nfpm-arch.yaml.new" "$WORK/nfpm-arch.yaml"
    if grep -q 'type: tree' "$WORK/nfpm-arch.yaml"; then
      echo "error: couldn't list the Arch package's files" >&2; exit 1
    fi
    # Without a fixed time nfpm stamps the files and .MTREE a moment apart, and `pacman -Qkk`
    # reports every file as modified. The commit time also makes the package reproducible.
    epoch="${SOURCE_DATE_EPOCH:-$(git -C "$ROOT" log -1 --format=%ct 2>/dev/null || date +%s)}"
    (cd "$ROOT" && SOURCE_DATE_EPOCH="$epoch" nfpm package -f "$WORK/nfpm-arch.yaml" -p archlinux -t "$DIST/$BASENAME.pkg.tar.zst")
  fi
fi

# ---- AppImage -----------------------------------------------------------------------------------
if has appimage; then
  APPDIR="$WORK/PhotoCraft.AppDir"
  cp -R "$STAGE" "$APPDIR"
  mv "$APPDIR/usr/share/doc" "$WORK/doc-unused"
  # AppRun is a script, not a symlink: it installs the .desktop entry and icons into
  # $XDG_DATA_HOME before launching, so Wayland compositors can resolve the window's
  # app_id to our icon instead of the generic Wayland logo (#593; opt-out env in the
  # script header). Logic is covered by packaging/linux/apprun-test.sh.
  install -Dm755 "$HERE/AppRun" "$APPDIR/AppRun"
  cp "$HERE/$APP_ID.desktop" "$APPDIR/$APP_ID.desktop"
  cp "$ROOT/assets/app-icon/hicolor/256x256/apps/$APP_ID.png" "$APPDIR/$APP_ID.png"
  ln -s "$APP_ID.png" "$APPDIR/.DirIcon"

  # Digests from the official GitHub release asset metadata:
  # https://github.com/AppImage/appimagetool/releases/tag/1.9.1
  # https://github.com/AppImage/type2-runtime/releases/tag/20251108
  # Keep both pins together:
  # appimagetool otherwise downloads the mutable latest type2 runtime itself.
  TOOL_VERSION=1.9.1
  RUNTIME_VERSION=20251108
  case "$ARCH" in
    x86_64)
      TOOL_SHA256=ed4ce84f0d9caff66f50bcca6ff6f35aae54ce8135408b3fa33abfc3cb384eb0
      RUNTIME_SHA256=2fca8b443c92510f1483a883f60061ad09b46b978b2631c807cd873a47ec260d
      ;;
    aarch64)
      TOOL_SHA256=f0837e7448a0c1e4e650a93bb3e85802546e60654ef287576f46c71c126a9158
      RUNTIME_SHA256=00cbdfcf917cc6c0ff6d3347d59e0ca1f7f45a6df1a428a0d6d8a78664d87444
      ;;
  esac
  mkdir -p "$CARGO_TARGET_DIR"
  TOOL="${APPIMAGETOOL:-$CARGO_TARGET_DIR/appimagetool-$TOOL_VERSION-$ARCH.AppImage}"
  if [ -n "${APPIMAGETOOL:-}" ]; then
    verify_appimagetool_override "$TOOL" "$TOOL_SHA256" "$TOOL_VERSION" "$ARCH"
  else
    download_verified \
      "https://github.com/AppImage/appimagetool/releases/download/$TOOL_VERSION/appimagetool-$ARCH.AppImage" \
      "$TOOL" "$TOOL_SHA256" true
  fi
  RUNTIME="$CARGO_TARGET_DIR/type2-runtime-$RUNTIME_VERSION-$ARCH"
  download_verified \
    "https://github.com/AppImage/type2-runtime/releases/download/$RUNTIME_VERSION/runtime-$ARCH" \
    "$RUNTIME" "$RUNTIME_SHA256"
  # Absolute, because appimagetool runs in $DIST below (CARGO_TARGET_DIR or APPIMAGETOOL may be
  # relative, e.g. target/agent-<name>).
  OUT="$(cd "$DIST" && pwd)/$BASENAME.AppImage"
  APPDIR="$(cd "$APPDIR" && pwd)"
  TOOL="$(cd "$(dirname "$TOOL")" && pwd)/$(basename "$TOOL")"
  RUNTIME="$(cd "$(dirname "$RUNTIME")" && pwd)/$(basename "$RUNTIME")"
  # A .zsync left by an earlier run would hide a missing zsyncmake and describe another file.
  rm -f "$OUT.zsync"
  # Update information (#349): AppImageUpdate, AppImageLauncher and the like read it from the
  # file and fetch only the blocks that changed in a newer release, through the .zsync published
  # next to each AppImage on GitHub Releases. `latest` is the newest published release that is
  # not a pre-release. A fork's builds point at its own releases through GITHUB_REPOSITORY.
  REPO="${GITHUB_REPOSITORY:-storytold/photocraft}"
  UPDATE_INFO="gh-releases-zsync|${REPO%%/*}|${REPO#*/}|latest|photocraft-*-linux-$ARCH.AppImage.zsync"
  # Extract-and-run: works without FUSE (containers, CI). The output embeds the static runtime,
  # so users don't need libfuse2 either. With zsyncmake on the host (CI installs the zsync
  # package) appimagetool also writes the .zsync, into its working directory, hence the cd.
  (cd "$DIST" && ARCH="$ARCH" APPIMAGE_EXTRACT_AND_RUN=1 "$TOOL" --no-appstream \
    --runtime-file "$RUNTIME" -u "$UPDATE_INFO" "$APPDIR" "$OUT")
  echo "wrote $OUT"
  if [ -s "$OUT.zsync" ]; then
    echo "wrote $OUT.zsync"
  else
    warn "zsyncmake not found, so $OUT.zsync was not written; AppImage delta updates need it"
  fi
fi

if [ -z "${CROSS_TARGET:-}" ]; then
  "$STAGE/usr/bin/photocraft-cli" --version
elif [ -n "${EMULATOR:-}" ]; then
  "$EMULATOR" "$STAGE/usr/bin/photocraft-cli" --version
fi
echo "==> done"
ls -lh "$DIST"
