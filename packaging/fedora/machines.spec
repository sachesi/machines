%define _debugsource_template %{nil}
%define debug_package %{nil}

%global app_id io.github.sachesi.machines

Name:           machines
# The release workflow and Copr set Version to the tag they build.
Version:        0.1.0
Release:        1%{?dist}
Summary:        Manager for libvirt virtual machines

License:        GPL-3.0-or-later
URL:            https://github.com/sachesi/machines
Source0:        %{url}/archive/refs/tags/v%{version}.tar.gz#/%{name}-%{version}.tar.gz
# The crates the build needs, from the release, so that it runs without a network.
Source1:        %{url}/releases/download/v%{version}/%{name}-%{version}-vendor.tar.xz

BuildRequires:  cargo
BuildRequires:  rust >= 1.92
BuildRequires:  gcc
BuildRequires:  blueprint-compiler
BuildRequires:  desktop-file-utils
BuildRequires:  gettext
BuildRequires:  appstream
BuildRequires:  pkgconfig(gtk4) >= 4.22
BuildRequires:  pkgconfig(libadwaita-1) >= 1.9
BuildRequires:  pkgconfig(glib-2.0)
BuildRequires:  pkgconfig(libvirt) >= 6.0.0
BuildRequires:  pkgconfig(gvnc-1.0)
BuildRequires:  pkgconfig(spice-client-glib-2.0) >= 0.39
# spice-client-glib-2.0.pc names libusb among the libraries it links with.
BuildRequires:  pkgconfig(libusb-1.0)
BuildRequires:  pkgconfig(vte-2.91-gtk4)

Requires:       gtk4%{?_isa} >= 4.22
Requires:       libadwaita%{?_isa} >= 1.9
Requires:       hicolor-icon-theme
# The daemons and QEMU for machines on this computer; a remote connection needs neither.
Recommends:     libvirt-daemon-kvm
Recommends:     libvirt-daemon-config-network
# Recognizing the system on an installation ISO.
Recommends:     osinfo-db
# UEFI firmware, an emulated TPM, and folders shared with a machine.
%ifarch x86_64
Recommends:     edk2-ovmf
%endif
%ifarch aarch64
Recommends:     edk2-aarch64
%endif
Recommends:     swtpm-tools
Recommends:     virtiofsd

%description
Machines manages the QEMU/KVM virtual machines of a libvirt connection, the
system one or your user session, built with GTK 4 and libadwaita. It starts,
stops, pauses and saves machines, shows their display in a built-in VNC or
SPICE console and their serial console as text, creates them from an
installation ISO or a disk image, clones them, takes snapshots of them, and
changes their hardware: disks, network interfaces, host USB and PCI devices,
processors, memory and firmware. It also manages storage pools and virtual
networks.

%prep
%autosetup -n %{name}-%{version} -b 1

%build
export CARGO_HOME="$PWD/.cargo-home"
export RUSTFLAGS="%{?build_rustflags}"
export MACHINES_LOCALEDIR="%{_datadir}/locale"
%if 0%{?_cargo_target_dir:1}
export CARGO_TARGET_DIR="%{_cargo_target_dir}"
%endif
cargo build --release --offline --locked

%install
%if 0%{?_cargo_target_dir:1}
target="%{_cargo_target_dir}/release"
%else
target="target/release"
%endif
install -Dpm 0755 "$target/machines" %{buildroot}%{_bindir}/machines

install -d %{buildroot}%{_datadir}/applications %{buildroot}%{_metainfodir}
msgfmt --desktop --template=data/%{app_id}.desktop -d po \
  -o %{buildroot}%{_datadir}/applications/%{app_id}.desktop
msgfmt --xml --template=data/%{app_id}.metainfo.xml -d po \
  -o %{buildroot}%{_metainfodir}/%{app_id}.metainfo.xml
install -Dpm 0644 data/%{app_id}.gschema.xml %{buildroot}%{_datadir}/glib-2.0/schemas/%{app_id}.gschema.xml
install -Dpm 0644 data/icons/hicolor/scalable/apps/%{app_id}.svg \
  %{buildroot}%{_datadir}/icons/hicolor/scalable/apps/%{app_id}.svg
install -Dpm 0644 data/icons/hicolor/symbolic/apps/%{app_id}-symbolic.svg \
  %{buildroot}%{_datadir}/icons/hicolor/symbolic/apps/%{app_id}-symbolic.svg

for lang in $(cat po/LINGUAS); do
  install -d %{buildroot}%{_datadir}/locale/$lang/LC_MESSAGES
  msgfmt -o %{buildroot}%{_datadir}/locale/$lang/LC_MESSAGES/%{name}.mo po/$lang.po
done
# find_lang fails where there is no translation yet, and rpm refuses an empty list, so the
# list starts with the binary.
echo %{_bindir}/machines > %{name}.lang
if [ -s po/LINGUAS ]; then %find_lang %{name}; fi

%check
desktop-file-validate %{buildroot}%{_datadir}/applications/%{app_id}.desktop
appstreamcli validate --no-net %{buildroot}%{_metainfodir}/%{app_id}.metainfo.xml
test -x %{buildroot}%{_bindir}/machines

%files -f %{name}.lang
%license LICENSE
%doc README.md
%{_datadir}/applications/%{app_id}.desktop
%{_metainfodir}/%{app_id}.metainfo.xml
%{_datadir}/glib-2.0/schemas/%{app_id}.gschema.xml
%{_datadir}/icons/hicolor/scalable/apps/%{app_id}.svg
%{_datadir}/icons/hicolor/symbolic/apps/%{app_id}-symbolic.svg

%changelog
