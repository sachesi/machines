//! What this computer has for passing its devices through to a machine: graphics cards,
//! kvmfr devices for Looking Glass, keyboards and mice.
//!
//! These are read from `/sys` and `/dev`, so they are the host's only where libvirt runs
//! on this computer.

use std::fs;
use std::io;
use std::os::fd::AsRawFd;

use crate::host_xml::PciAddress;

/// Whether the host's PCI device at `address` is a graphics card, which it is whether it
/// is bound to its own driver or to the one that passes it through.
pub fn is_graphics_card(address: &PciAddress) -> bool {
    // The PCI class 0x03 is a display controller.
    fs::read_to_string(format!("/sys/bus/pci/devices/{address}/class"))
        .is_ok_and(|c| c.starts_with("0x03"))
}

/// A kvmfr device, which Looking Glass shares the guest's screen through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Kvmfr {
    pub path: String,
    /// Its size, or why it could not be read.
    pub bytes: Result<u64, String>,
}

/// The kvmfr module's `KVMFR_DMABUF_GETSIZE` request, `_IO('u', 0x44)`.
const KVMFR_DMABUF_GETSIZE: libc::Ioctl = 0x7544;

/// The kvmfr devices, with the size each has, which the module gives only through an
/// ioctl on the device, as the Looking Glass client reads it. Reading it takes opening
/// the device, which this user may not be allowed.
pub fn kvmfr_devices() -> Vec<Kvmfr> {
    let Ok(entries) = fs::read_dir("/dev") else {
        return Vec::new();
    };
    let mut numbers: Vec<u32> = entries
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.strip_prefix("kvmfr")?.parse().ok())
        .collect();
    numbers.sort_unstable();
    numbers
        .into_iter()
        .map(|n| {
            let path = format!("/dev/kvmfr{n}");
            let bytes = fs::File::open(&path)
                .and_then(|f| {
                    // SAFETY: the request takes no argument and only returns the size.
                    match unsafe { libc::ioctl(f.as_raw_fd(), KVMFR_DMABUF_GETSIZE, 0) } {
                        -1 => Err(io::Error::last_os_error()),
                        size => Ok(size),
                    }
                })
                .map_err(|e| e.to_string())
                .and_then(|size| match u64::try_from(size) {
                    Ok(0) | Err(_) => Err("it has no memory".to_owned()),
                    Ok(bytes) => Ok(bytes),
                });
            Kvmfr { path, bytes }
        })
        .collect()
}

/// A keyboard or mouse, by the stable path udev gives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputDevice {
    pub path: String,
    pub name: String,
}

/// The host's keyboards, and its mice.
pub fn input_devices() -> (Vec<InputDevice>, Vec<InputDevice>) {
    let mut keyboards = Vec::new();
    let mut mice = Vec::new();
    let Ok(entries) = fs::read_dir("/dev/input/by-id") else {
        return (keyboards, mice);
    };
    let mut files: Vec<String> = entries
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    files.sort();
    for file in files {
        let (list, stem) = if let Some(stem) = file.strip_suffix("-event-kbd") {
            (&mut keyboards, stem)
        } else if let Some(stem) = file.strip_suffix("-event-mouse") {
            (&mut mice, stem)
        } else {
            continue;
        };
        list.push(InputDevice {
            path: format!("/dev/input/by-id/{file}"),
            name: input_name(stem),
        });
    }
    (keyboards, mice)
}

/// Whether this user may read the device, as QEMU has to where it runs as this user.
/// Opening an input device takes nothing from the host; only a grab would.
pub fn readable(path: &str) -> bool {
    fs::File::open(path).is_ok()
}

/// "usb-Logitech_USB_Receiver-if02" as "Logitech USB Receiver".
fn input_name(stem: &str) -> String {
    let stem = stem.split_once('-').map_or(stem, |(_, rest)| rest);
    let stem = match stem.rsplit_once("-if") {
        Some((name, interface)) if interface.chars().all(|c| c.is_ascii_digit()) => name,
        _ => stem,
    };
    stem.replace('_', " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_names_leave_out_the_bus_and_interface() {
        assert_eq!(
            input_name("usb-Logitech_USB_Receiver-if02"),
            "Logitech USB Receiver"
        );
        assert_eq!(input_name("usb-Keychron_K2"), "Keychron K2");
        assert_eq!(input_name("platform-i8042"), "i8042");
    }
}
