# Show Desktop — COSMIC Applet

A COSMIC panel applet that minimizes all open windows to show the desktop. Click again to restore them.

## Features

- **Toggle** — click the applet icon to minimize/restore all windows
- **Keyboard shortcut** — bind `cosmic-ext-show-desktop --toggle` to any key combination
- **Icon feedback** — icon changes when desktop is visible

## Install

### From source

```sh
git clone https://github.com/Gargadon/cosmic-ext-show-desktop
cd cosmic-ext-show-desktop
./build.sh
```

> **Note:** Uses Git dependencies (`libcosmic`, `cosmic-protocols`) pinned to specific revisions — network required.

### Arch Linux (AUR)

A PKGBUILD is included in the repository. Build with `makepkg -si`.

## Usage

1. Add **Show Desktop** to your panel:  
   *COSMIC Settings → Desktop → Panel → Add applet*
2. Click the desktop icon in the panel to minimize all windows
3. Click again to restore them

### Keyboard shortcut (optional)

Configure a custom shortcut in *COSMIC Settings → Keyboard → Keyboard Shortcuts*:

- **Command:** `cosmic-ext-show-desktop --toggle`
- **Key:** e.g. <kbd>Super</kbd>+<kbd>D</kbd>

The `--toggle` flag sends a signal to the running applet via a Unix socket (`$XDG_RUNTIME_DIR/show-desktop-cosmic.sock`).

## Build requirements

- Rust toolchain
- `libxkbcommon`, `fontconfig`, COSMIC libraries (provided by `cosmic-randr`)
