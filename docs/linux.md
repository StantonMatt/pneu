# Linux

Pneu supports KDE Plasma 6 on Wayland. The x86_64 release binaries are built on
Ubuntu 24.04 for a glibc 2.39 / Qt 6.4 baseline; Ubuntu 24.04+, Kubuntu 24.10+
and Debian 13 can install them (Ubuntu/Kubuntu 24.04's stock Plasma 5 is not a
supported desktop). No Rust toolchain is needed for a release download.

## Install

Download `pneu_<version>_amd64.deb` from the
[latest release](https://github.com/StantonMatt/pneu/releases/latest), then:

```sh
sudo apt install ./pneu_*_amd64.deb
```

Open **Pneu** in the app menu. It starts the systemd user service in the current
session; future graphical logins start it automatically. Closing the window
keeps transfers running. The package enables the unit globally with Debian's
`deb-systemd-helper --user`; maintainer scripts never start or restart user
services. On upgrade, restart your own service or log out and back in to run
the new binary.

For other distributions, download
[pneu-linux-x86_64.tar.gz](https://github.com/StantonMatt/pneu/releases/latest/download/pneu-linux-x86_64.tar.gz),
install the Qt 6 Core/Gui/Widgets/Network/DBus libraries and Qt Wayland/QPA plugins
with your package manager, then:

```sh
tar -xzf pneu-linux-x86_64.tar.gz
cd pneu-linux-x86_64
./install.sh
# Later, from the same extracted directory:
./install.sh --uninstall
```

Use a regular user, without sudo. The installer writes `~/.local/bin/pneud`,
desktop integration under `${XDG_DATA_HOME:-~/.local/share}` and a user unit
under `${XDG_CONFIG_HOME:-~/.config}/systemd/user`. It reenables the service and
restarts it if a graphical session is active. Add `~/.local/bin` to PATH if your
desktop session does not already include it. Tray SVGs are embedded in the
binary and written into Pneu's data directory at startup.

Source installation remains available from a checkout:

```sh
cd desktop
QMAKE=qmake6 ./install.sh
# Uninstall without rebuilding:
./install.sh --uninstall
```

Building needs Rust 1.93+, a C++ compiler, pkg-config, Qt 6 development tools
including qmake6, and fontconfig, Wayland, xkbcommon and udev development headers.
The installed service needs a systemd user session. A file chooser needs
`xdg-desktop-portal` and your desktop's portal backend.

## Switching installations and removing Pneu

A pre-existing `~/.config/systemd/user/pneud.service` (or XDG config equivalent)
from a source/tar installation overrides the packaged system unit. Its
`ExecStart` keeps pointing to `~/.local/bin/pneud`; a matching per-user desktop
entry also takes precedence. Installing the .deb deliberately leaves all these
files and the running source installation alone. To switch, use the new
source or tarball installer's `./install.sh --uninstall` (older installers may
not support that flag), then install the .deb and open Pneu. If the .deb is already installed, uninstall the per-user copy and run
`systemctl --user start pneud.service` afterward. A customized unit must be
reviewed and removed manually; the per-user uninstaller refuses to remove it.

Remove the package with `sudo apt remove pneu`, or `sudo apt purge pneu` to
also discard Debian's unit-enable state. Removal stops future global starts,
and reinstalling restores them; it does not stop an already-running daemon. Before removing, close Pneu and run
`systemctl --user stop pneud.service` in your session. Neither uninstall route
deletes pairings, configuration, caches or received files.

## Compositor support

Supported: Plasma 6 Wayland; GNOME/Mutter lacks data-control, so file transfers can run but automatic clipboard sync and clipboard writes are unavailable.

As checked on 2026-10-30, upstream Mutter has neither
`ext-data-control-v1` nor `wlr-data-control-unstable-v1`: its
[Wayland protocol build list](https://github.com/GNOME/mutter/blob/432c167ef2de37a708140b02e29881646fc9c13c/src/meson.build)
and [Wayland implementation](https://github.com/GNOME/mutter/tree/432c167ef2de37a708140b02e29881646fc9c13c/src/wayland)
contain neither implementation. This is an upstream-source check, not a GNOME
session test. Pneu uses wl-clipboard-rs data-control, not a GNOME extension.
Watcher initialization failure is logged as `clipboard watch unavailable` and
retried independently of the transfer server. Clipboard actions may report
errors, including copying received images; file saving and transport continue.
Other GNOME desktop integration has not been validated.

KDE documents system service menus in
[`/usr/share/kio/servicemenus`](https://develop.kde.org/docs/apps/dolphin/service-menus/).

## Building packages

On Ubuntu 24.04, install the source build dependencies plus `dpkg-dev`, then:

```sh
cd desktop
QMAKE=qmake6 cargo build --release --locked -p pneud
packaging/linux/build-packages.sh target/release/pneud 2.2.1 /path/to/output
```

The script strips a copy of the supplied binary, derives library dependencies
with `dpkg-shlibdeps` (retaining minimum versions across the Qt Gui/Widgets
package-name alternatives used by these distributions), adds Qt's dynamically
loaded QPA/Wayland plugins, and
produces the .deb and tarball. Set `SOURCE_DATE_EPOCH` for repeatable archive
metadata; it defaults to the checkout's last commit time. Identical binary,
inputs, packaging tools and epoch produce identical artifacts. Build on the
oldest supported baseline: packaging a newer host binary cannot lower its
runtime requirements. Release CI uses the pinned `ubuntu-24.04` runner.
