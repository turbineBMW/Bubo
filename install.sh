#!/usr/bin/env bash
# User-local install: binary, .desktop, icons, login unit. No root needed.
set -euo pipefail
cd "$(dirname "$0")"
cargo build --release
install -Dm755 target/release/bubo ~/.local/bin/bubo
# Launchers often lack ~/.local/bin on PATH, and .desktop files don't expand ~,
# so bake the absolute binary path into the installed copy.
mkdir -p ~/.local/share/applications
sed "s#^Exec=.*#Exec=$HOME/.local/bin/bubo#" data/dev.turbinebmw.Bubo.desktop \
  > ~/.local/share/applications/dev.turbinebmw.Bubo.desktop
chmod 644 ~/.local/share/applications/dev.turbinebmw.Bubo.desktop
for n in 16 32 48 64 128 256 512; do
  install -Dm644 data/icons/bubo-$n.png ~/.local/share/icons/hicolor/${n}x${n}/apps/dev.turbinebmw.Bubo.png
done
# The user unit "Start at login" enables on a desktop without the Background portal;
# enabling it is up to the user, from Preferences.
mkdir -p ~/.local/share/systemd/user
sed "s#@bindir@#$HOME/.local/bin#" data/dev.turbinebmw.Bubo.user-service.in \
  > ~/.local/share/systemd/user/dev.turbinebmw.Bubo.service
systemctl --user daemon-reload >/dev/null 2>&1 || true
gtk4-update-icon-cache -q ~/.local/share/icons/hicolor 2>/dev/null || true
update-desktop-database ~/.local/share/applications 2>/dev/null || true
echo "installed: ~/.local/bin/bubo"
