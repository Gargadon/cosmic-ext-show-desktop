#!/usr/bin/env bash
set -euo pipefail

echo "==> Removing old installation..."

sudo rm -f /usr/bin/cosmic-ext-show-desktop
sudo rm -f /usr/bin/show-desktop-cosmic
sudo rm -f /usr/share/applications/com.system76.CosmicAppletShowDesktop.desktop

echo "==> Done. Reinstall with: sudo ./build.sh"
