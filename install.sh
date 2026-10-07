#!/bin/sh
# sway-blur installer: system libraries + latest prebuilt binary.
#
#   curl -fsSL https://raw.githubusercontent.com/developic/sway-blur/main/install.sh | bash
set -eu

REPO="developic/sway-blur"
BIN="sway-blur"
BINDIR="/usr/local/bin"

# ── UI kit: headers, status lines, progress bar ─────────────────────────────────
if [ -t 1 ] && [ -z "${NO_COLOR:-}" ] && [ "${TERM:-}" != "dumb" ]; then
  RST="$(printf '\033[0m')"; DIM="$(printf '\033[2m')"; BLD="$(printf '\033[1m')"
  GRN="$(printf '\033[32m')"; YEL="$(printf '\033[33m')"; RED="$(printf '\033[31m')"
  CYA="$(printf '\033[36m')"
else
  RST=''; DIM=''; BLD=''; GRN=''; YEL=''; RED=''; CYA=''
fi
TICK='✓'; ARROW='→'; HBAR='─'; BARW=20

ok()   { printf '  %s[ ok ]%s %s\n' "$GRN" "$RST" "${1:-}"; }
skip() { printf '  %s[ -- ]%s %s\n' "$DIM" "$RST" "${1:-}"; }
warn() { printf '  %s[warn]%s %s\n' "$YEL" "$RST" "${1:-}" >&2; }
die()  { printf '  %s[fail]%s %s\n' "$RED" "$RST" "${1:-fatal error}"; exit 1; }
header() { printf '\n  %s%s[%s/%s]%s %s\n' "$BLD" "$CYA" "${1:-?}" "${2:-?}" "$RST" "${3:-}"; }
human() {
  _h_b=$(printf '%s' "${1:-0}" | tr -dc '0-9'); _h_b=${_h_b:-0}
  awk -v b="$_h_b" 'BEGIN{ if(b<1024) printf "%d B", b; else if(b<1048576) printf "%.1f KB", b/1024; else printf "%.1f MB", b/1048576 }'
}
have() { command -v "$1" >/dev/null 2>&1; }

# ── download animation: progress bar ──────────────────────────────────────────

bar() { # bar <pct> → BARW-cell bar on stdout
  _bar_pct=${1:-0}; _bar_done=$((_bar_pct * BARW / 100)); _bar_i=0; _bar_s=
  while [ "$_bar_i" -lt "$BARW" ]; do
    if [ "$_bar_i" -lt "$_bar_done" ]; then _bar_s="${_bar_s}${HBAR}"; else _bar_s="${_bar_s} "; fi
    _bar_i=$((_bar_i + 1))
  done
  printf '%s' "$_bar_s"
}

fetch() { # fetch <url> <outfile> <label>
  _f_url=${1:-}; _f_out=${2:-}; _f_label=${3:-download}
  _f_total=$(curl -fsSIL --proto '=https' --tlsv1.2 --max-time 20 "$_f_url" 2>/dev/null \
    | tr -d '\r' | sed -n 's/^[Cc]ontent-[Ll]ength: *//p' | tail -n 1 | tr -dc '0-9')
  _f_total=${_f_total:-0}
  : >"$_f_out"
  curl -fsSL --proto '=https' --tlsv1.2 --retry 3 --retry-delay 1 \
    -w '%{size_download} %{speed_download}' \
    -o "$_f_out" "$_f_url" >"$_f_out.metrics" 2>"$_f_out.err" &
  _f_pid=$!
  while kill -0 "$_f_pid" 2>/dev/null; do
    _f_have=$(wc -c <"$_f_out" 2>/dev/null | tr -dc '0-9'); _f_have=${_f_have:-0}
    _f_pct=0
    if [ "$_f_total" -gt 0 ]; then
      _f_pct=$((_f_have * 100 / _f_total))
      if [ "$_f_pct" -gt 100 ]; then _f_pct=100; fi
    fi
    if [ "$_f_total" -gt 0 ]; then _f_ttotal=$(human "$_f_total"); else _f_ttotal="?"; fi
    _f_bar=$(bar "$_f_pct"); _f_have_h=$(human "$_f_have")
    printf '\r  %s %s%s%s %s%3d%%%s  %s/%s%s' \
      "$_f_label" "$CYA" "$_f_bar" "$RST" "$DIM" "$_f_pct" "$RST" \
      "$_f_have_h" "$_f_ttotal" "$RST"
    sleep 0.1 2>/dev/null || true
  done
  _f_st=0
  wait "$_f_pid" || _f_st=$?
  printf '\r\033[K'
  if [ "$_f_st" -ne 0 ]; then
    tail -n 3 "$_f_out.err" 2>/dev/null >&2 || true
    rm -f "$_f_out.metrics" "$_f_out.err" 2>/dev/null
    die "download failed: $_f_url"
  fi
  _f_mz=0; _f_md=0
  IFS=' ' read -r _f_mz _f_md <"$_f_out.metrics" 2>/dev/null || true   # curl -w has no trailing \n
  _f_mz=$(printf '%s' "$_f_mz" | tr -dc '0-9'); _f_mz=${_f_mz:-0}
  # %{speed_download} is float (bytes/sec); drop the fraction for arithmetic
  _f_md=${_f_md%%.*}
  _f_mk=$(printf '%s' "$_f_md" | tr -dc '0-9'); _f_mk=${_f_mk:-0}
  _f_rate=$(awk -v k="$_f_mk" 'BEGIN{ if(k>=1048576) printf "%.1f MB/s", k/1048576; else if(k>=1024) printf "%.0f KB/s", k/1024; else printf "%d B/s", k }')
  _f_full=$(bar 100); _f_size=$(human "$_f_mz")
  printf '  %s %s%s%s %s%3d%%%s  %s  %s%s\n' \
    "$_f_label" "$CYA" "$_f_full" "$RST" "$GRN" 100 "$RST" \
    "$_f_rate" "$GRN" "$_f_size$RST"
  rm -f "$_f_out.metrics" "$_f_out.err" 2>/dev/null
}

# ── startup standards: fail fast before doing anything ─────────────────────────
# Only tar/xz are checked: curl got us here, the rest ships with every distro.
have tar || die "missing required command: tar"
have xz || die "missing required command: xz (needed to extract .tar.xz releases)"

printf '%s' "$BLD"
cat <<'EOF'
  ____                      _     _
 / ___|_      ____ _ _   _ | |__ | |_   _
 \___ \ \ /\ / / _` | | | || |_ \| | | | |
  ___) |\ V  V / (_| | |_| || |_) | | |_| |
 |____/  \_/\_/ \__,_|\__,_||_.__/|_|\__,_|
EOF
printf '%s' "$RST"
printf '%ssway-blur installer%s\n\n' "$DIM" "$RST"

SUDO=
if [ "$(id -u)" != "0" ] && have sudo; then
  SUDO=sudo
fi

header 1 3 "System libraries"
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

_m=$(uname -m)
case "$_m" in
  x86_64) ARCH="x86_64" ;;
  aarch64) ARCH="aarch64" ;;
  *) die "unsupported architecture: $_m (need x86_64 or aarch64)" ;;
esac
[ "$(uname -s)" = "Linux" ] || die "only Linux is supported"

header 2 3 "Latest release"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT INT TERM
# Latest tag via the releases/latest redirect — no API quota, no JSON parsing.
TAG=""; VERSION=""
_redir=$(curl -fsSL -o /dev/null -w '%{url_effective}' \
  --proto '=https' --tlsv1.2 --retry 2 --max-time 20 \
  "https://github.com/${REPO}/releases/latest" 2>/dev/null) || _redir=""
case "$_redir" in
  */tag/*)
    TAG=${_redir##*/tag/}
    TAG=${TAG%%\?*}
    TAG=${TAG%/}
    ;;
esac
VERSION=${TAG#v}
[ -n "$VERSION" ] || die "could not resolve latest release — check the network"
ok "latest release: $TAG"

TARBALL="${BIN}-${VERSION}-linux-${ARCH}.tar.xz"
fetch "https://github.com/${REPO}/releases/download/${TAG}/${TARBALL}" "${TMP}/${TARBALL}" "$TARBALL"

header 3 3 "Install"
tar -xJf "${TMP}/${TARBALL}" -C "$TMP"
$SUDO install -m 0755 "${TMP}/${BIN}" "${BINDIR}/${BIN}"
ok "${BINDIR}/${BIN} (v${VERSION})"

printf '\n  %s%s%s  %s%s v%s%s\n' "$GRN" "$TICK" "$RST" "$BLD" "$BIN" "$VERSION" "$RST"
printf '  %s  run %s%s%s to start the daemon in this session\n' "$ARROW" "$CYA" "$BIN" "$RST"
printf '  %s  add %sexec_always %s/%s%s to your sway config to autostart\n' "$ARROW" "$CYA" "$BINDIR" "$BIN" "$RST"
printf '  %s  binary lives at %s%s/%s%s\n' "$ARROW" "$CYA" "$BINDIR" "$BIN" "$RST"
