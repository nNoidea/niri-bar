Name:           niri-bar
Version:        {{{ sed -n 's/^version = "\(.*\)"/\1/p' "$(git rev-parse --show-toplevel)/Cargo.toml" | head -n 1 }}}
Release:        1%{?dist}
Summary:        Fast, lightweight Wayland status bar and spatial window taskbar for Niri

License:        GPL-3.0-or-later
URL:            https://github.com/nNoidea/niri-bar
Source0:        {{{ git_dir_pack }}}

BuildRequires:  cargo
BuildRequires:  rust >= 1.85
BuildRequires:  pkgconfig(gtk+-3.0)
BuildRequires:  pkgconfig(gtk-layer-shell-0)
BuildRequires:  gcc

# Runtime dependencies
Requires:       gtk3
Requires:       gtk-layer-shell

# Optional modules (gracefully hidden when hardware is not present)
Recommends:     pipewire-utils
Recommends:     bluez

%description
A fast, lightweight Wayland status bar and spatial window taskbar built
specifically for the Niri scrollable-tiling compositor.

%prep
{{{ git_dir_setup_macro }}}

%build
cargo build --release --locked

%install
install -D -p -m 0755 target/release/niri-bar %{buildroot}%{_bindir}/niri-bar
install -D -p -m 0644 resources/config.default.toml %{buildroot}%{_datadir}/niri-bar/config.default.toml
install -D -p -m 0644 resources/style.default.css %{buildroot}%{_datadir}/niri-bar/style.default.css
install -D -p -m 0644 resources/NerdFont-Regular.ttf %{buildroot}%{_datadir}/fonts/niri-bar/NerdFont-Regular.ttf
install -D -p -m 0644 images/icon2.svg %{buildroot}%{_datadir}/icons/hicolor/scalable/apps/niri-bar.svg

%files
%license LICENSE
%doc README.md
%{_bindir}/niri-bar
%{_datadir}/niri-bar/
%{_datadir}/fonts/niri-bar/
%{_datadir}/icons/hicolor/scalable/apps/niri-bar.svg

%changelog
* Sun Oct 11 2026 nNoidea <https://github.com/nNoidea/niri-bar> - 0.2.1-1
- Initial packaging
