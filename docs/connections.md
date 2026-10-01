# Connections

Machines works with one libvirt connection at a time. The main menu switches between the
two on this computer; the choice is kept in the `connection-uri` setting.

## System

`qemu:///system`, the default, is the connection virt-manager uses. Its machines run as the
`qemu` user and can start with the computer. Using it takes either membership of the
`libvirt` group or a polkit agent to ask for your password, which every full desktop has.

QEMU has to be able to read the installation ISO as the `qemu` user. One kept under your
home folder usually cannot be read; put it in `/var/lib/libvirt/images` or another place
the `qemu` user can reach.

Virtual networks, passing PCI devices through, and the scripts libvirt runs as a machine
starts and stops are only on this connection.

## User session

`qemu:///session` needs neither the group nor a password: its machines run as you, with
QEMU's own user-mode networking in place of libvirt's virtual networks.

## Other connections

Any other libvirt URI, such as one over SSH to another computer, can be set in the
setting:

    gsettings set io.github.sachesi.machines connection-uri 'qemu+ssh://user@host/system'

On a connection to another computer, the console still works, through libvirt, and SPICE
can lend the machine USB devices of the computer Machines runs on. What reads this
computer's own devices or files, such as Looking Glass, the host's keyboard and mouse, and
the start and stop scripts, is not offered there.
