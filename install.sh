#!/bin/sh
# composemux installer — https://github.com/sofired/composemux
#
# Usage (as documented in the README):
#   curl -fsSL https://raw.githubusercontent.com/sofired/composemux/main/install.sh | sh
#
# What it does, in order:
#   1. Works out which prebuilt target matches this machine.
#   2. Finds the latest release tag (or honours COMPOSEMUX_VERSION).
#   3. Downloads the .tar.gz and its .sha256, and VERIFIES the checksum
#      before touching anything — a pipe-to-shell installer must.
#   4. Extracts and installs the binary into a no-sudo directory.
#
# It never runs downloaded content: the only thing executed is the verified
# composemux binary, and only after the checksum matches. Nothing is written
# outside a private temp dir until that check passes.
#
# Environment overrides:
#   COMPOSEMUX_VERSION      Pin a version (e.g. 0.1.0). Default: latest release.
#   COMPOSEMUX_INSTALL_DIR  Where to install. Default: $HOME/.local/bin.
#   COMPOSEMUX_API_URL      GitHub "latest release" JSON endpoint. For testing.
#   COMPOSEMUX_BASE_URL     Release-download base URL. For testing/mirrors.
#
# POSIX sh only — it is piped into `sh`. No bashisms; passes `shellcheck -s sh`.

set -eu

REPO="sofired/composemux"
BIN="composemux"
API_URL="${COMPOSEMUX_API_URL:-https://api.github.com/repos/${REPO}/releases/latest}"
BASE_URL="${COMPOSEMUX_BASE_URL:-https://github.com/${REPO}/releases/download}"
INSTALL_DIR="${COMPOSEMUX_INSTALL_DIR:-$HOME/.local/bin}"

info() { printf '%s\n' "$*"; }
err()  { printf '%s\n' "composemux install: $*" >&2; }
die()  { err "$*"; exit 1; }

# --- Tooling: prefer curl, fall back to wget; require a sha256 tool ----------

if command -v curl >/dev/null 2>&1; then
  DOWNLOADER=curl
elif command -v wget >/dev/null 2>&1; then
  DOWNLOADER=wget
else
  die "need curl or wget to download files, found neither"
fi

# macOS ships `shasum`; most Linux ships `sha256sum`. Prefer sha256sum, then
# fall back to `shasum -a 256`. Both print "<hex>  <file>", so the hash is the
# first whitespace-delimited field either way.
if command -v sha256sum >/dev/null 2>&1; then
  SHA_TOOL=sha256sum
elif command -v shasum >/dev/null 2>&1; then
  SHA_TOOL=shasum
else
  die "need sha256sum or shasum to verify the download, found neither"
fi

# download <url> <dest-file>. Fails non-zero on any HTTP or network error.
download() {
  if [ "$DOWNLOADER" = curl ]; then
    curl -fsSL "$1" -o "$2"
  else
    wget -q -O "$2" "$1"
  fi
}

# sha256 <file> — print "<hex>  <file>" using whichever tool we found.
sha256() {
  if [ "$SHA_TOOL" = sha256sum ]; then
    sha256sum "$1"
  else
    shasum -a 256 "$1"
  fi
}

# --- Target detection --------------------------------------------------------

# libc_variant — on x86_64 Linux we ship both a glibc (gnu) and a musl build.
# Prefer gnu, but pick musl when this system's C library is musl (e.g. Alpine),
# because a glibc-linked binary will not run there.
#
# Rule: `ldd --version` prints "ldd (GNU libc) ..." on glibc and a banner
# containing "musl" on musl. glibc prints it to stdout, musl to stderr, so we
# fold stderr into stdout (2>&1) and look for "musl". If ldd is unavailable, we
# fall back to probing for a musl dynamic loader on disk; absent both signals we
# assume gnu, the common case.
libc_variant() {
  if command -v ldd >/dev/null 2>&1 && ldd --version 2>&1 | grep -qi musl; then
    echo musl
    return
  fi
  for f in /lib/ld-musl-*.so.* /lib/libc.musl-*.so.*; do
    if [ -e "$f" ]; then
      echo musl
      return
    fi
  done
  echo gnu
}

# detect_target — echo the Rust target triple for this machine, or exit with a
# clear message for platforms we do not publish a prebuilt binary for. `uname`
# is overridable via the `uname` shell function tests can define.
detect_target() {
  os="$(uname -s)"
  arch="$(uname -m)"

  case "$arch" in
    x86_64 | amd64) arch=x86_64 ;;
    aarch64 | arm64) arch=arm64 ;;
  esac

  case "$os" in
    Linux)
      case "$arch" in
        x86_64) echo "x86_64-unknown-linux-$(libc_variant)" ;;
        arm64)  echo "aarch64-unknown-linux-gnu" ;;
        *) die "unsupported Linux architecture: $(uname -m)" ;;
      esac
      ;;
    Darwin)
      case "$arch" in
        arm64) echo "aarch64-apple-darwin" ;;
        x86_64)
          # No Intel-macOS build exists: GitHub retired its Intel runners, so
          # there is no machine to produce x86_64-apple-darwin. Do not hand an
          # Intel Mac the arm64 archive — send them to a from-source build.
          err "Intel Macs (x86_64-apple-darwin) have no prebuilt binary."
          err "Install from source with a Rust toolchain instead:"
          err "    cargo install ${BIN}"
          exit 1
          ;;
        *) die "unsupported macOS architecture: $(uname -m)" ;;
      esac
      ;;
    *)
      die "unsupported operating system: $os (this installer serves Linux and macOS)"
      ;;
  esac
}

# --- Version discovery -------------------------------------------------------

# All release tags are "vX.Y.Z" (release.yml triggers on "v*" and strips the
# leading "v" for asset names). We normalise any override the same way, so both
# `0.1.0` and `v0.1.0` work.
resolve_version() {
  if [ -n "${COMPOSEMUX_VERSION:-}" ]; then
    printf '%s\n' "${COMPOSEMUX_VERSION#v}"
    return
  fi
  json="$1"
  if ! download "$API_URL" "$json"; then
    die "could not reach the GitHub release API at $API_URL"
  fi
  # Extract tag_name without a jq dependency. GitHub returns it as
  #   "tag_name": "v0.1.0"
  tag="$(sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$json" | head -n1)"
  [ -n "$tag" ] || die "could not find tag_name in the release API response"
  printf '%s\n' "${tag#v}"
}

# --- Main --------------------------------------------------------------------

target="$(detect_target)"

# Private working dir, cleaned up on any exit so we never leave partial state.
tmp="$(mktemp -d 2>/dev/null || mktemp -d -t composemux)"
trap 'rm -rf "$tmp"' EXIT INT TERM HUP

version="$(resolve_version "$tmp/release.json")"
tag="v$version"

stem="${BIN}-${version}-${target}"
archive="${stem}.tar.gz"
archive_url="${BASE_URL}/${tag}/${archive}"
checksum_url="${archive_url}.sha256"

info "Installing ${BIN} ${version} (${target})"

# Download the archive and its published checksum.
if ! download "$archive_url" "$tmp/$archive"; then
  die "failed to download $archive_url"
fi
if ! download "$checksum_url" "$tmp/$archive.sha256"; then
  die "failed to download $checksum_url"
fi

# Verify BEFORE extracting. Compare only the hex digest: the published file
# records a bare filename (shasum was run inside dist/), which will not match
# our temp path under `shasum -c`, so we check the digests directly.
expected="$(awk '{print $1}' "$tmp/$archive.sha256")"
actual="$(sha256 "$tmp/$archive" | awk '{print $1}')"
[ -n "$expected" ] || die "published checksum file was empty"
if [ "$expected" != "$actual" ]; then
  err "checksum mismatch for $archive — refusing to install."
  err "  expected: $expected"
  err "  actual:   $actual"
  die "aborting; nothing was installed"
fi
info "Checksum verified."

# Extract into the temp dir. The archive holds a single directory,
#   composemux-<version>-<target>/composemux
tar xzf "$tmp/$archive" -C "$tmp"
src="$tmp/$stem/$BIN"
[ -f "$src" ] || die "archive did not contain $stem/$BIN"

# Install into a no-sudo directory. Write to a temp name in the same directory
# and mv into place so an interrupted copy cannot leave a half-written binary.
mkdir -p "$INSTALL_DIR"
dest="$INSTALL_DIR/$BIN"
tmp_dest="$dest.tmp.$$"
cp "$src" "$tmp_dest"
chmod 755 "$tmp_dest"
mv -f "$tmp_dest" "$dest"

info "Installed ${BIN} to ${dest}"

# PATH advice — we do not edit shell rc files silently.
case ":$PATH:" in
  *":$INSTALL_DIR:"*)
    info "Run '${BIN}' to get started."
    ;;
  *)
    info ""
    info "${INSTALL_DIR} is not on your PATH. Add it, e.g.:"
    info "    export PATH=\"${INSTALL_DIR}:\$PATH\""
    info "then restart your shell (or add that line to your shell's rc file)."
    ;;
esac
