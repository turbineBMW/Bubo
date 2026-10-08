#!/usr/bin/env bash
# Remove what install.sh put in place.
#   ./uninstall.sh            -> the app; your pairing and settings stay for a reinstall
#   ./uninstall.sh --purge    -> and unpair the phone, then remove settings, sign-in and caches
set -euo pipefail
APP_ID=dev.turbinebmw.Bubo
BIN=~/.local/bin/bubo
SHARE=~/.local/share

purge=false
for arg in "$@"; do
  case $arg in
    --purge) purge=true ;;
    -h|--help) sed -n '2,4p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown option: $arg (try --help)" >&2; exit 2 ;;
  esac
done

# The login unit first: disabled, so nothing links to it, and stopped, so it can't restart Bubo.
UNIT="$SHARE/systemd/user/$APP_ID.service"
if [ -e "$UNIT" ]; then
  systemctl --user disable --now "$APP_ID.service" >/dev/null 2>&1 || true
  rm -f "$UNIT"
  systemctl --user daemon-reload >/dev/null 2>&1 || true
fi
# "Start at login" without systemd left an autostart entry that would now fail at every login.
rm -f "${XDG_CONFIG_HOME:-$HOME/.config}/autostart/$APP_ID.desktop"

# A running Bubo (it may be in the background) quits the way Quit does. A build from before
# Bubo was single-instance isn't reachable that way, so it is stopped.
if pgrep -x bubo >/dev/null 2>&1; then
  gapplication action "$APP_ID" quit 2>/dev/null || true
  for _ in 1 2 3 4 5; do pgrep -x bubo >/dev/null 2>&1 || break; sleep 1; done
  pkill -x bubo 2>/dev/null || true
fi

# Before the binary goes: deleting auth.json alone would leave Bubo listed on the phone as a
# paired device. Unpairing revokes it there, as "Unpair phone" in the menu does.
if $purge && [ -x "$BIN" ] && [ -e "${XDG_CONFIG_HOME:-$HOME/.config}/bubo/auth.json" ]; then
  timeout 30 "$BIN" unpair 2>/dev/null \
    || echo "Could not unpair (offline?); remove Bubo on the phone: Messages → Device pairing."
fi

rm -f "$BIN"
rm -f "$SHARE/applications/$APP_ID.desktop"
for n in 16 32 48 64 128 256 512; do
  rm -f "$SHARE/icons/hicolor/${n}x${n}/apps/$APP_ID.png"
done
gtk4-update-icon-cache -q "$SHARE/icons/hicolor" 2>/dev/null || true
update-desktop-database "$SHARE/applications" 2>/dev/null || true

if $purge; then
  rm -rf "${XDG_CONFIG_HOME:-$HOME/.config}/bubo"   # pairing, settings
  rm -rf "${XDG_DATA_HOME:-$HOME/.local/share}/bubo" # the Google sign-in's WebKit data
  rm -rf "${XDG_CACHE_HOME:-$HOME/.cache}/bubo"      # log, avatars, WebKit cache
  echo "Removed Bubo's pairing, settings, Google sign-in and caches."
else
  echo "Your pairing and settings are kept in ~/.config/bubo; --purge removes them."
fi
echo "uninstalled: $BIN"
