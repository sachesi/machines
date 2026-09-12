# Machines

Machines manages QEMU/KVM virtual machines through libvirt, written in Rust with GTK 4 and
libadwaita. It lists the machines of the system connection or of your user session,
starts, stops and pauses them, shows their display in a built-in VNC or SPICE console,
creates new ones from an installation ISO or an existing disk image, and deletes them with
their disks.

A machine's details page adds and removes its disks, CD/DVD drives and network
interfaces, gives it whole disks of the host, and passes USB and PCI devices of the host
through to it. It also picks the display protocol and video card (virtio, QXL, VGA…), and
turns on 3D acceleration, which renders on the host's GPU with a virtio card. What a running
machine cannot take at once, such as a SATA drive, it gets at its next start. The main
menu also manages the connection's storage pools, with the volumes in them, and its
virtual networks.

The console reaches a machine's display through libvirt rather than over the network, so
new machines have a display with no listening socket at all: SPICE where
QEMU has it, which carries sound and, with the SPICE agent in the guest, sizes the guest's
screen to the window; VNC otherwise. With 3D acceleration, SPICE hands the console the GPU's
frames as they are, while VNC has QEMU read them back.

You need libvirt with its QEMU driver, gvnc (from gtk-vnc), spice-glib, GTK 4.22 and
libadwaita 1.9.

## Building and installing

On Fedora the build needs:

    sudo dnf install cargo blueprint-compiler gtk4-devel libadwaita-devel libvirt-devel \
        gvnc-devel spice-glib-devel libusb1-devel

Then:

    just build
    sudo just install        # or: just prefix=$HOME/.local install

`just run` starts the debug build without installing it, and `just check` and `just test`
are what a change has to pass.

## Connections

The system connection (`qemu:///system`) is the one virt-manager uses, and the default. Its
machines run as the `qemu` user and can start with the host. Using it takes either
membership of the `libvirt` group or a polkit agent to ask for your password, which every
full desktop has. The user session (`qemu:///session`) needs neither: its machines run as
you, with QEMU's own user-mode networking in place of libvirt's virtual networks. The
main menu switches between the two.

Virtual networks, and passing PCI devices through, need the system connection. A PCI
device also needs the IOMMU turned on in the firmware and on the kernel command line
(`intel_iommu=on` on Intel), and the host goes without the device while the machine has it.

On the system connection, QEMU runs as `qemu` and has to be able to read the installation
ISO. One kept under your home folder usually cannot be read by it; put it in
`/var/lib/libvirt/images` or another place the `qemu` user can reach.

## License

GPL-3.0-or-later. Contact: sachesi <xsachesi@pm.me>.
