//! What this computer has for passing its devices through to a machine: graphics cards,
//! kvmfr devices for Looking Glass, keyboards and mice.
//!
//! These are read from `/sys` and `/dev`, so they are the host's only where libvirt runs
//! on this computer.

use std::fs;
use std::path::Path;

/// How many graphics cards the host has, bound to a driver or to none; one of them is
/// passed through only where there is another for the host.
pub fn graphics_cards() -> usize {
    let Ok(entries) = fs::read_dir("/sys/bus/pci/devices") else {
        return 0;
    };
    entries
        .flatten()
        .filter(|e| {
            // The PCI class 0x03 is a display controller.
            fs::read_to_string(e.path().join("class")).is_ok_and(|c| c.starts_with("0x03"))
        })
        .count()
}

/// A kvmfr device, which Looking Glass shares the guest's screen through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Kvmfr {
    pub path: String,
    pub bytes: u64,
}

/// The kvmfr devices, which the module makes one of for each size it is loaded with.
pub fn kvmfr_devices() -> Vec<Kvmfr> {
    let Ok(sizes) = fs::read_to_string("/sys/module/kvmfr/parameters/static_size_mb") else {
        return Vec::new();
    };
    kvmfr_sizes(&sizes)
        .into_iter()
        .filter(|k| Path::new(&k.path).exists())
        .collect()
}

/// The devices the sizes in MiB, "32,64", make: `/dev/kvmfr0` of the first, and on.
fn kvmfr_sizes(sizes: &str) -> Vec<Kvmfr> {
    sizes
        .trim()
        .split(',')
        .enumerate()
        .filter_map(|(i, mib)| {
            Some(Kvmfr {
                path: format!("/dev/kvmfr{i}"),
                bytes: mib.trim().parse::<u64>().ok().filter(|&m| m > 0)? << 20,
            })
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
    fn kvmfr_devices_follow_the_sizes() {
        assert_eq!(
            kvmfr_sizes("32,128\n"),
            [
                Kvmfr {
                    path: "/dev/kvmfr0".to_owned(),
                    bytes: 32 << 20
                },
                Kvmfr {
                    path: "/dev/kvmfr1".to_owned(),
                    bytes: 128 << 20
                },
            ]
        );
        assert!(kvmfr_sizes("\n").is_empty());
    }

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
