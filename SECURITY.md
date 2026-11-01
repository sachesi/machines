# Security

Please report a vulnerability privately rather than in a public issue: through
[a private advisory](https://github.com/sachesi/machines/security/advisories/new) on GitHub,
or by mail to sachesi <xsachesi@pm.me>. Say what you found, how to reproduce it and which
version you ran; a fix is worked out with you before anything is published.

Only the latest release gets fixes.

## What counts

Machines manages virtual machines through libvirt, and a small part of it runs as root.
The parts where a mistake matters most:

- The helper `machines-hooks`, which pkexec runs as root under the polkit action
  `io.github.sachesi.machines.hooks` once an administrator's password is given. It takes a
  UUID, a stage and a script, and writes only under `/etc/machines/scripts` and its one
  dispatcher (see [docs/scripts.md](docs/scripts.md)). Reaching it without that password,
  or making it write any other file or follow a symbolic link, is a vulnerability.
- The dispatcher it installs in `/etc/libvirt/hooks/qemu.d`, which libvirt runs as root
  for every machine on the system connection. A machine's name or definition that makes it
  run anything but the script kept for that machine and stage is a vulnerability.
- The consoles, where what arrives over VNC, SPICE, Looking Glass and the serial console
  comes from the guest. A guest that makes Machines run code or read the host's files
  through them, or takes the microphone, the clipboard or a USB device that its settings
  do not give it, is a vulnerability.

A script an administrator saved doing what it says is not a vulnerability, since running
it as root is its purpose. Neither is what a user can already do through libvirt: on the
system connection, membership of the `libvirt` group is close to root by design. A flaw in
libvirt, QEMU or a console protocol's library belongs with that project.

A machine that fails to start or a page that shows the wrong value is a bug; please file
it as an ordinary issue.
