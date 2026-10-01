# Metadata

What libvirt has no element for, Machines keeps in the `<metadata>` of a machine's
definition, in the namespace `https://github.com/sachesi/machines/metadata/1`:

```xml
<metadata>
  <machines:machine xmlns:machines="https://github.com/sachesi/machines/metadata/1">
    <machines:host-microphone/>
  </machines:machine>
</metadata>
```

| Element | Means |
|---------|-------|
| `host-microphone` | The console gives the machine this computer's microphone. |
| `reset-nvram` | The firmware variables are made afresh from their template at the next start, after a change between UEFI with and without Secure Boot. Machines takes it out once the machine has started. |
| `video-before-looking-glass` | The video card model the machine had before Looking Glass took it away, to put back when Looking Glass is turned off. |
| `accel3d-before-looking-glass` | That video card had 3D acceleration. |

The elements are in the definition libvirt keeps for the next start, which
`virsh dumpxml --inactive` shows, or:

    virsh metadata --config MACHINE https://github.com/sachesi/machines/metadata/1
