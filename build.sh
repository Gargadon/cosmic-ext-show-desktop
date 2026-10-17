#!/usr/bin/env bash
set -euo pipefail

echo "==> Building cosmic-ext-show-desktop..."
cd -- "$(dirname -- "${BASH_SOURCE[0]}")"
cargo build --release --locked

echo "==> Installing binary..."
sudo install -Dm755 target/release/cosmic-ext-show-desktop /usr/bin/cosmic-ext-show-desktop

echo "==> Installing desktop entry..."
sudo install -Dm644 data/com.system76.CosmicAppletShowDesktop.desktop /usr/share/applications/com.system76.CosmicAppletShowDesktop.desktop

echo "==> Done! Add 'Show Desktop' to your panel via COSMIC Settings -> Desktop -> Panel -> Add applet"
