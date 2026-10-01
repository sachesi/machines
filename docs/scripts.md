# Start and stop scripts

On the system connection to this computer, the Scripts group of a machine's details
(shown with Advanced Settings, or once the machine has a script) edits two scripts libvirt
runs as root:

- **Before It Starts**, at libvirt's `prepare` stage; if it fails, the machine does not
  start
- **After It Stops**, at libvirt's `release` stage, once libvirt has taken back what it gave
  the machine

Each gets the machine's name as its first argument, followed by the rest of libvirt's hook
arguments, and the machine's definition on standard input. A script must not call libvirt,
which waits for it to finish.

## What is installed

Saving a script runs the helper `machines-hooks`, from `libexec`, through pkexec, which asks
for an administrator's password under the polkit action
`io.github.sachesi.machines.hooks`. The helper:

- writes the script to `/etc/machines/scripts/UUID/prepare` or `.../release`, owned by root
  and executable; this folder, outside libvirt's own, can be read by everyone, so the app
  shows the scripts without a password
- installs, the first time, the dispatcher `/etc/libvirt/hooks/qemu.d/machines`, which
  runs the script for the machine and stage libvirt calls it for, and reloads `virtqemud`
  or `libvirtd`, which only look for hooks as they start
- removes the script when it is saved empty, and the machine's folder once it is empty

It takes nothing but a UUID and a stage, and a script of at most 64 KiB of UTF-8 text that
starts with `#!`. It writes no other file, refuses symbolic links in the paths it writes,
and replaces a script whole, never half-written.

## Removing them

Deleting `/etc/machines/scripts` and `/etc/libvirt/hooks/qemu.d/machines` removes every
script and the dispatcher. `just uninstall` and the packages leave both in place, as they
hold what the user wrote.
