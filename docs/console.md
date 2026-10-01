# The console

A running machine's page shows its display. Machines reaches the display through libvirt,
which hands over a socket already past authentication, so a display needs no listening
socket, and new machines are made without one. The console speaks SPICE where QEMU has its
SPICE modules, and VNC otherwise.

## Keyboard and pointer

A click in the console gives it the keyboard, with the shortcuts the desktop would
otherwise take, such as Super and Alt+Tab; every key goes to the machine, Tab included.
Pressing Ctrl+Alt together and letting go gives the keyboard back.

F11 makes the console fullscreen while the console does not have the keyboard. In
fullscreen, the machine's controls, with keys to send, come down from the top edge of the
screen. The console can also move to a window of its own, and save the machine's screen as
a screenshot.

## SPICE

With SPICE, the console also:

- plays the machine's sound
- gives the machine this computer's microphone, only where the Microphone switch in the
  machine's display settings allows it; it is off for every machine until turned on, and
  changing it reconnects the console
- shares text copied on either side, with the SPICE agent in the guest; what is copied on
  this computer is offered to the guest only once the console has the keyboard
- asks the guest, through the agent, to take the size of the console
- lends the machine USB devices of the computer Machines runs on, on any connection
- shows the frames the host's GPU renders as they are, with 3D acceleration

## Looking Glass

A machine whose screen is a passed-through graphics card's can show that screen in the
console through [Looking Glass](https://looking-glass.io), with the Looking Glass switch in
the Passthrough group of its details. The guest runs the Looking Glass host application,
version B7, and the console reads its screen from the kvmfr device, which the user running
Machines has to be able to open. The keyboard and mouse go over SPICE. Scroll Lock works as
in the Looking Glass client; held down, it lists the keys that go with it.

## Serial console

The third page shows the machine's serial console as text, for machines without a
display and for watching a guest boot. New machines have a serial port for it.
