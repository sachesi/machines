# Installing

## Packages

Each release on [GitHub](https://github.com/sachesi/machines/releases) carries packages for
Fedora, Debian, Ubuntu and Arch Linux, and the crates the build needs, for building without
a network. The same release is built in:

- Copr, for Fedora: the [sachesi/software](https://copr.fedorainfracloud.org/coprs/sachesi/software/)
  project, package `machines`
- the openSUSE Build Service: [home:sachesi:software](https://build.opensuse.org/package/show/home:sachesi:software/machines)
- the AUR: [machines](https://aur.archlinux.org/packages/machines)

## Requirements

Machines needs GTK 4.22, libadwaita 1.9, libvirt with its QEMU driver, gvnc (from
gtk-vnc), spice-glib, VTE for GTK 4 and GtkSourceView 5. The rest is optional:

- osinfo-db, to recognize the system on an installation ISO and give a new machine the
  memory, disk and firmware that system recommends
- OVMF (edk2), for UEFI firmware
- swtpm, for an emulated TPM
- virtiofsd, for folders shared with a machine
- pkexec, to save the scripts libvirt runs as a machine starts and stops
- QEMU's SPICE modules, for a SPICE display; without them, machines get VNC

## Building from source

The build needs Rust 1.92 or newer, [just](https://github.com/casey/just),
blueprint-compiler, gettext, and the development files of the libraries above and of
libusb. On Fedora:

    sudo dnf install cargo just blueprint-compiler gettext gtk4-devel libadwaita-devel \
        libvirt-devel gvnc-devel spice-glib-devel libusb1-devel vte291-gtk4-devel \
        gtksourceview5-devel

Then:

    just build
    sudo just install        # prefix /usr/local
    just prefix=$HOME/.local install

`install` copies what `build` made and builds nothing, so the two can run on different
machines sharing the source folder. It installs:

- `bin/machines`
- `libexec/machines-hooks`, the helper that writes the start and stop scripts
- the polkit policy for that helper, in `/usr/share/polkit-1/actions` whatever the prefix,
  as polkit reads policies from there alone; without it, pkexec still runs the helper and
  asks in its own words
- the desktop file, metainfo, GSettings schema, icons and translations

`DESTDIR` stages the install for a package, and leaves the schema, desktop and icon caches
to the package manager. `just uninstall` removes what `install` put in place.

## Development

- `just run` starts the debug build without installing it.
- `just check` runs rustfmt, clippy, cargo-deny, the Blueprint compiler and the desktop
  file and metainfo validators.
- `just test` runs the tests.
- `just pot` regenerates `po/machines.pot`, and `just po` merges it into each translation.

A change has to pass `just check` and `just test`, which CI runs on Fedora.
