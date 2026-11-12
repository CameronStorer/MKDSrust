MKDSrust for Linux x86_64

Extract this archive and run ./MKDSrust. Follow Setup to choose your own clean
USA revision 0 cartridge dump (AMCE), install the private Rust toolchain, and
build the game locally. No Rust installation is required.

Before the first build, install a C compiler, pkg-config and the ALSA and udev
development packages (Setup checks and shows the command for your distro):
  Debian/Ubuntu: sudo apt install build-essential pkg-config libasound2-dev libudev-dev
  Fedora:        sudo dnf install gcc pkgconf-pkg-config alsa-lib-devel systemd-devel
  Arch:          sudo pacman -S --needed base-devel alsa-lib systemd-libs
  openSUSE:      sudo zypper install gcc pkg-config alsa-devel libudev-devel
Needs glibc 2.31 or newer (Ubuntu 20.04+, Debian 11+, Fedora 32+), X11 or
Wayland, and internet for the official Rust archives and crates.io on the
first build. Allow several GB of free disk space and time for compilation.

The launcher contains project source, not a ROM, Nintendo game assets or
translated game code. These are produced only on your computer from your ROM.
Do not distribute generated games, generated source or extracted assets.

Everything is kept next to the launcher in data/ (settings, controls, saves,
toolchain, cache) and games/ (your builds), so move or delete the folder as a
whole. If the folder is not writable, ~/.local/share/MKDSrust is used instead.
The launcher adds an "MKDSrust" menu entry and handles mkdsrust:// mod links;
the game window shows your cartridge's icon.

GPL-3.0-or-later. See LICENSE. Independent project, not affiliated with or
endorsed by Nintendo. Nintendo trademarks belong to their respective owner.
