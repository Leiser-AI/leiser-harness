#!/bin/sh
# Installs the newest leiser-harness on macOS or Linux:
#
#   curl -fsSL https://raw.githubusercontent.com/Leiser-AI/leiser-harness/development/install.sh | sh
#
# It downloads the installer of the latest release with curl. macOS checks
# only files a browser marked as downloaded (quarantine), so the app then
# starts without the warning an app not notarized by Apple gets. Run again
# to update; the app also updates itself.
#
# LEISER_INSTALL_DIR installs the macOS app somewhere else than
# /Applications (used by the tests); LEISER_INSTALL_NO_OPEN=1 does not start
# it afterwards.
set -eu

DOWNLOAD="https://github.com/Leiser-AI/leiser-harness/releases/latest/download"
APP="leiser-harness.app"

say() { printf '%s\n' "$*"; }
fail() { printf 'leiser-harness: %s\n' "$*" >&2; exit 1; }

work=$(mktemp -d)
# apt reads the package as its own user.
chmod 755 "$work"
cleanup() {
  # Never delete into a disk image that is still mounted.
  if [ -n "${volume:-}" ] && ! hdiutil detach -quiet -force "$volume"; then return; fi
  rm -rf "$work"
}
trap cleanup EXIT
trap 'exit 130' INT TERM

fetch() {
  say "Downloading $1 ..."
  curl -fL --progress-bar -o "$work/$1" "$DOWNLOAD/$1" || fail "could not download $DOWNLOAD/$1"
}

install_macos() {
  target=${LEISER_INSTALL_DIR:-/Applications}
  fetch leiser-harness-macos.dmg
  volume="$work/volume"
  hdiutil attach -nobrowse -quiet -mountpoint "$volume" "$work/leiser-harness-macos.dmg" ||
    fail "could not open the disk image"
  # A running app would keep its old files; the harness service keeps
  # running and takes the new version once no turn runs.
  if [ -z "${LEISER_INSTALL_DIR:-}" ] && pgrep -x leiser-desktop >/dev/null; then
    osascript -e 'quit app id "ai.leiser.harness"' 2>/dev/null || true
  fi
  mkdir -p "$target"
  rm -rf "${target:?}/$APP"
  ditto "$volume/$APP" "$target/$APP" || fail "could not copy the app to $target"
  say "Installed $target/$APP"
  if [ "${LEISER_INSTALL_NO_OPEN:-}" != 1 ]; then open "$target/$APP"; fi
}

install_linux() {
  [ "$(uname -m)" = x86_64 ] || fail "the Linux packages are for x86_64 only, not $(uname -m)"
  if command -v apt-get >/dev/null 2>&1; then
    fetch leiser-harness-linux.deb
    sudo apt-get update
    sudo apt-get install -y "$work/leiser-harness-linux.deb"
  elif command -v dnf >/dev/null 2>&1; then
    # Betas come without an .rpm (RPM versions cannot hold their `-`).
    curl -fsIL -o /dev/null "$DOWNLOAD/leiser-harness-linux.rpm" ||
      fail "there is no .rpm yet: it comes with the first stable release. Until then, use a Debian- or Ubuntu-based system."
    fetch leiser-harness-linux.rpm
    sudo dnf install -y "$work/leiser-harness-linux.rpm"
  else
    fail "this installs with apt (Debian, Ubuntu) or dnf (Fedora); see https://github.com/Leiser-AI/leiser-harness#install"
  fi
  say "Installed leiser-harness; start it from your app menu."
}

case "$(uname -s)" in
  Darwin) install_macos ;;
  Linux) install_linux ;;
  *) fail "this script is for macOS and Linux; on Windows, see https://github.com/Leiser-AI/leiser-harness#install" ;;
esac
