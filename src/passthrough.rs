//! What this computer has for passing its devices through to a machine: graphics cards,
//! kvmfr devices for Looking Glass, keyboards and mice.
//!
//! These are read from `/sys` and `/dev`, so they are the host's only where libvirt runs
//! on this computer.

use std::fs;
use std::io;
use std::os::fd::AsRawFd;

use crate::glib;

/// All of it at once, read off the main loop, as reading it opens some of the devices.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Devices {
    /// The graphics cards, by PCI address as sysfs names them.
    pub graphics_cards: Vec<String>,
    pub kvmfr: Vec<Kvmfr>,
    /// Whether the Looking Glass client is installed.
    pub looking_glass_client: bool,
    pub keyboards: Vec<InputDevice>,
    pub mice: Vec<InputDevice>,
}

impl Devices {
    /// `qemu_is_other_user` where QEMU runs as a user of its own, which may open every
    /// keyboard and mouse.
    pub fn read(qemu_is_other_user: bool) -> Self {
        let (keyboards, mice) = input_devices(qemu_is_other_user);
        Self {
            graphics_cards: graphics_cards(),
            kvmfr: kvmfr_devices(),
            looking_glass_client: glib::find_program_in_path("looking-glass-client").is_some(),
            keyboards,
            mice,
        }
    }
}

/// The PCI devices that are graphics cards, whether bound to their own driver or to the
/// one that passes them through.
fn graphics_cards() -> Vec<String> {
    let Ok(entries) = fs::read_dir("/sys/bus/pci/devices") else {
        return Vec::new();
    };
    let mut cards: Vec<String> = entries
        .flatten()
        // The PCI class 0x03 is a display controller.
        .filter(|e| fs::read_to_string(e.path().join("class")).is_ok_and(|c| c.starts_with("0x03")))
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    cards.sort();
    cards
}

/// A kvmfr device, which Looking Glass shares the guest's screen through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Kvmfr {
    pub path: String,
    /// Its size, or why it could not be read.
    pub bytes: Result<u64, String>,
}

/// The kvmfr module's `KVMFR_DMABUF_GETSIZE` request, `_IO('u', 0x44)`.
const KVMFR_DMABUF_GETSIZE: libc::c_ulong = 0x7544;

/// The kvmfr devices, with the size each has, which the module gives only through an
/// ioctl on the device, as the Looking Glass client reads it. Reading it takes opening
/// the device, which this user may not be allowed.
fn kvmfr_devices() -> Vec<Kvmfr> {
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
                    // Through syscall, as ioctl() cuts the size, a long, to an int.
                    // SAFETY: the request takes no argument and only returns the size.
                    let size = unsafe {
                        libc::syscall(
                            libc::SYS_ioctl,
                            f.as_raw_fd(),
                            KVMFR_DMABUF_GETSIZE,
                            0 as libc::c_ulong,
                        )
                    };
                    match size {
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
    /// Whether QEMU may open it.
    pub usable: bool,
}

/// The host's keyboards, and its mice.
fn input_devices(qemu_is_other_user: bool) -> (Vec<InputDevice>, Vec<InputDevice>) {
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
        let path = format!("/dev/input/by-id/{file}");
        list.push(InputDevice {
            usable: qemu_is_other_user || readable(&path),
            path,
            name: input_name(stem),
        });
    }
    (keyboards, mice)
}

/// Whether this user may read the device, as QEMU has to where it runs as this user.
/// Opening an input device takes nothing from the host; only a grab would.
fn readable(path: &str) -> bool {
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
