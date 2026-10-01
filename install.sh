#!/bin/sh
# sway-blur installer: system libraries + latest prebuilt binary.
#
#   curl -fsSL https://raw.githubusercontent.com/developic/sway-blur/main/install.sh | bash
set -eu

REPO="developic/sway-blur"
BIN="sway-blur"
BINDIR="/usr/local/bin"

if [ -t 1 ] && [ -z "${NO_COLOR:-}" ] && [ "${TERM:-}" != "dumb" ]; then
  BLD="$(printf '\033[1m')"
  DIM="$(printf '\033[2m')"
  CYAN="$(printf '\033[36m')"
  GREEN="$(printf '\033[32m')"
  YELLOW="$(printf '\033[33m')"
  RED="$(printf '\033[31m')"
  RESET="$(printf '\033[0m')"
else
  BLD='' DIM='' CYAN='' GREEN='' YELLOW='' RED='' RESET=''
fi

stage() { printf '%s[+]%s %s\n' "$CYAN" "$RESET" "$*"; }
ok() { printf '%s[✓]%s %s\n' "$GREEN" "$RESET" "$*"; }
skip() { printf '%s[-]%s %s\n' "$DIM" "$RESET" "$*"; }
warn() { printf '%s[!]%s %s\n' "$YELLOW" "$RESET" "$*" >&2; }
die() { printf '%s[!] error: %s%s\n' "$RED" "$RESET" "$*" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

have curl || die "curl is required"
have tar || die "tar is required"

printf '%s' "$BLD"
cat <<'EOF'
  ____                      _     _
 / ___|_      ____ _ _   _ | |__ | |_   _
 \___ \ \ /\ / / _` | | | || |_ \| | | | |
  ___) |\ V  V / (_| | |_| || |_) | | |_| |
 |____/  \_/\_/ \__,_|\__,_||_.__/|_|\__,_|
EOF
printf '%s' "$RESET"
printf '%ssway-blur installer%s\n\n' "$DIM" "$RESET"

SUDO=
if [ "$(id -u)" != "0" ] && have sudo; then
  SUDO=sudo
fi

stage "[1/3] System libraries"
if have apt-get; then
  MISSING=
  for p in libglib2.0-0 libgtk-3-0 libgtk-layer-shell0; do
    dpkg -s "$p" >/dev/null 2>&1 || MISSING="$MISSING $p"
  done
  if [ -z "$MISSING" ]; then
    skip "already present"
  else
    # shellcheck disable=SC2086
    $SUDO apt-get install -y --no-install-recommends $MISSING
    ok "installed"
  fi
elif have dnf; then
  MISSING=
  for p in glib2 gtk3 gtk-layer-shell; do
    rpm -q "$p" >/dev/null 2>&1 || MISSING="$MISSING $p"
  done
  if [ -z "$MISSING" ]; then
    skip "already present"
  else
    # shellcheck disable=SC2086
    $SUDO dnf install -y $MISSING
    ok "installed"
  fi
elif have pacman; then
  MISSING=
  for p in glib2 gtk3 gtk-layer-shell; do
    pacman -Q "$p" >/dev/null 2>&1 || MISSING="$MISSING $p"
  done
  if [ -z "$MISSING" ]; then
    skip "already present"
  else
    # shellcheck disable=SC2086
    $SUDO pacman -S --noconfirm --needed $MISSING
    ok "installed"
  fi
else
  warn "no supported package manager; install GTK3 + gtk-layer-shell manually"
fi

case "$(uname -m)" in
  x86_64 | amd64) ARCH="x86_64" ;;
  aarch64 | arm64) ARCH="aarch64" ;;
  *) die "unsupported architecture: $(uname -m) (need x86_64 or aarch64)" ;;
esac
[ "$(uname -s)" = "Linux" ] || die "only Linux is supported"

stage "[2/3] Download latest release"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT INT TERM
cd "$TMP"
curl -fsSL -o api.json "https://api.github.com/repos/${REPO}/releases/latest"
VERSION="$(grep -m1 '"tag_name"' api.json | cut -d'"' -f4 | sed 's/^v//')"
[ -n "$VERSION" ] || die "could not resolve latest release"

TARBALL="${BIN}-${VERSION}-linux-${ARCH}.tar.xz"
BASE_URL="https://github.com/${REPO}/releases/download/v${VERSION}"
curl -fsSL -o "$TARBALL" "${BASE_URL}/${TARBALL}"
curl -fsSL -o "${TARBALL}.sha256" "${BASE_URL}/${TARBALL}.sha256"
sha256sum -c "${TARBALL}.sha256" >/dev/null && ok "${TARBALL} (checksum ok)"

stage "[3/3] Install"
tar -xJf "$TARBALL" -C "$TMP"
$SUDO install -m 0755 "${TMP}/${BIN}" "${BINDIR}/${BIN}"
ok "${BINDIR}/${BIN} (v${VERSION})"

printf '\n%ssway-blur v%s ready — run: %s%s\n' "$GREEN" "$VERSION" "$BIN" "$RESET"
printf '%sautostart: exec_always %s/%s%s\n' "$DIM" "$BINDIR" "$BIN" "$RESET"
