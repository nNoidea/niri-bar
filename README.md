# niri-bar

A fast, lightweight Wayland status bar and spatial window taskbar built in Rust **SPECIFICALLY for the [Niri](https://github.com/YaLTeR/niri) scrollable-tiling compositor.**

> **Why niri-bar instead of any other bar?**  

niri-bar is built from the ground up, only for Niri. When I started using Niri, I saw that no other taskbar properly took advantage of the Niri IPC, instead resorting to hacks and workarounds that constantly broke. I spent endless time fixing their hacks and piling on my own band-aids only for them to break again. So niri-bar was born: built for Niri, and only for Niri.

[![CI](https://github.com/nNoidea/niri-bar/actions/workflows/ci.yml/badge.svg)](https://github.com/nNoidea/niri-bar/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/nNoidea/niri-bar)](https://github.com/nNoidea/niri-bar/releases)
![License: GPL v3](https://img.shields.io/badge/License-GPLv3-blue.svg)

## Features

- **Spatial Taskbar**: Real-time window tracking via Niri IPC with **drag-and-drop** and app icons.
- **System Tray**: StatusNotifierItem (SNI) implementation with DBusMenu and GTK popup support.
- **Built-in Modules**: Volume, Brightness, Bluetooth, Network, Battery, Memory, Clock, and Spacer.
- **Display Awareness**: Automatic multi-monitor reconciliation and layer-shell integration.
- **Customizable**: TOML configuration and GTK CSS styling. Horizontal and Vertical layout support.

## Requirements

### Runtime
- [Niri](https://github.com/YaLTeR/niri) Wayland compositor
- `gtk3`
- `gtk-layer-shell`

### Build (compiling from source)
- Rust toolchain (1.75+)
- `gcc` or `clang`, `pkg-config`, `make`
- GTK3 & gtk-layer-shell development packages:
  - **Fedora**: `sudo dnf install gtk3-devel gtk-layer-shell-devel pkg-config`
  - **Arch Linux**: `sudo pacman -S gtk3 gtk-layer-shell`
  - **Debian / Ubuntu**: `sudo apt install libgtk-3-dev libgtk-layer-shell-dev pkg-config`

## Installation

### Prebuilt Binary
Download the precompiled binary from [Releases](https://github.com/nNoidea/niri-bar/releases/latest) and place it in your `PATH` (e.g. `~/.cargo/bin` or `/usr/local/bin`).

### From Source
```bash
git clone https://github.com/nNoidea/niri-bar.git
cd niri-bar
make build
make install
```
Default installation installs to `~/.cargo/bin/niri-bar`. To install system-wide to `/usr/local/bin`:
```bash
sudo make install PREFIX=/usr/local
```

Or install directly with Cargo:
```bash
cargo install --git https://github.com/nNoidea/niri-bar
```

## Usage

Add `niri-bar` to your Niri configuration (`~/.config/niri/config.kdl`):

```kdl
spawn-at-startup "niri-bar"
```

## Configuration

On first launch, default configuration and stylesheet templates are created at:
- `~/.config/niri-bar/config.toml`
- `~/.config/niri-bar/style.css`

For reference on available configuration options and CSS classes, see:
- [`resources/config.default.toml`](resources/config.default.toml)
- [`resources/style.default.css`](resources/style.default.css)

## License

GPL-3.0-or-later. See [LICENSE](LICENSE) for details.
