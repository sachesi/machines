//! Writes the scripts libvirt runs as root when a virtual machine starts and stops, for the
//! app to run through pkexec.
//!
//! `machines-hooks set UUID EVENT` makes the script on standard input the one for the
//! machine UUID at EVENT, `prepare` or `release`; an empty one removes it. Nothing else is
//! accepted: no paths, no other files, and scripts only under [`SCRIPTS`], which libvirt
//! reaches through the one dispatcher at [`DISPATCHER`].

use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::Path;
use std::process::{Command, ExitCode, Stdio};

/// Where libvirt runs every executable file for each QEMU machine's events.
const DISPATCHER: &str = "/etc/libvirt/hooks/qemu.d/machines";
/// The scripts, in a folder for each machine by UUID; outside libvirt's own folder, which
/// only root may read, so that the app shows them without asking for a password.
const SCRIPTS: &str = "/etc/machines/scripts";
const EVENTS: [&str; 2] = ["prepare", "release"];
/// More than any script needs, and less than a mistake could fill the disk with.
const MAX_SCRIPT: u64 = 64 * 1024;

/// Runs the script for the machine and event libvirt calls it for, with the machine's
/// definition on standard input as libvirt gives it; a script's failure at `prepare` keeps
/// the machine from starting.
const DISPATCHER_SCRIPT: &str = r#"#!/bin/sh
# Installed by Machines: runs the script it keeps for this virtual machine and event.
case "$2/$3" in
prepare/begin) event=prepare ;;
release/end) event=release ;;
*) exit 0 ;;
esac
definition=$(cat)
uuid=$(printf '%s\n' "$definition" | sed -n 's|^ *<uuid>\([0-9a-f-]*\)</uuid> *$|\1|p' | head -n 1)
script=/etc/machines/scripts/$uuid/$event
[ -n "$uuid" ] && [ -x "$script" ] || exit 0
printf '%s\n' "$definition" | "$script" "$@"
"#;

fn main() -> ExitCode {
    match run(std::env::args().skip(1).collect()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("machines-hooks: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: Vec<String>) -> Result<(), String> {
    let [command, uuid, event] = args.as_slice() else {
        return Err("usage: machines-hooks set UUID EVENT < script".to_owned());
    };
    if command != "set" {
        return Err(format!("unknown command {command}"));
    }
    if !is_uuid(uuid) {
        return Err(format!("{uuid} is not a UUID"));
    }
    if !EVENTS.contains(&event.as_str()) {
        return Err(format!("{event} is not one of {}", EVENTS.join(", ")));
    }
    // SAFETY: geteuid has no preconditions and cannot fail.
    if unsafe { libc::geteuid() } != 0 {
        return Err("it has to run as root".to_owned());
    }
    let mut script = Vec::new();
    io::stdin()
        .take(MAX_SCRIPT + 1)
        .read_to_end(&mut script)
        .map_err(|e| format!("the script cannot be read: {e}"))?;
    let script = checked_script(script)?;
    // SAFETY: umask has no preconditions and cannot fail. Whatever root's own, the scripts
    // stay readable for the app to show them.
    unsafe { libc::umask(0o022) };

    let folder = format!("{SCRIPTS}/{uuid}");
    let path = format!("{folder}/{event}");
    match script {
        Some(script) => {
            install_dispatcher()?;
            make_folder("/etc/machines")?;
            make_folder(SCRIPTS)?;
            make_folder(&folder)?;
            write_executable(&path, script.as_bytes())
        }
        None => {
            remove(&path)?;
            // Only once empty; a failure to remove it is no failure to remove the script.
            let _ = fs::remove_dir(&folder);
            Ok(())
        }
    }
}

/// Whether `text` is a UUID as libvirt writes it: lowercase, in five groups.
fn is_uuid(text: &str) -> bool {
    let groups: Vec<&str> = text.split('-').collect();
    groups.iter().map(|g| g.len()).eq([8, 4, 4, 4, 12])
        && groups.iter().all(|g| {
            g.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
}

/// The script to write, or `None` to remove it: it has to be text a kernel can run.
fn checked_script(script: Vec<u8>) -> Result<Option<String>, String> {
    if script.len() as u64 > MAX_SCRIPT {
        return Err(format!("the script is larger than {MAX_SCRIPT} bytes"));
    }
    let script = String::from_utf8(script).map_err(|_| "the script is not UTF-8 text")?;
    if script.trim().is_empty() {
        return Ok(None);
    }
    if script.contains('\0') {
        return Err("the script holds a NUL character".to_owned());
    }
    if !script.starts_with("#!") {
        return Err("the script does not start with #!, naming what runs it".to_owned());
    }
    Ok(Some(script))
}

/// Refuse a path that is a symbolic link, which could point the write anywhere.
fn no_link(path: &str) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => Err(format!("{path} is a symbolic link")),
        _ => Ok(()),
    }
}

fn make_folder(path: &str) -> Result<(), String> {
    no_link(path)?;
    match fs::DirBuilder::new().mode(0o755).create(path) {
        Err(e) if e.kind() != io::ErrorKind::AlreadyExists => Err(format!("{path}: {e}")),
        _ if !Path::new(path).is_dir() => Err(format!("{path} is not a folder")),
        _ => Ok(()),
    }
}

/// Write `contents` to `path` whole or not at all, owned by root and executable.
fn write_executable(path: &str, contents: &[u8]) -> Result<(), String> {
    no_link(path)?;
    let temporary = format!("{path}.new");
    let _ = fs::remove_file(&temporary);
    let write = || -> io::Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o755)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&temporary)?;
        file.write_all(contents)?;
        file.sync_all()?;
        fs::rename(&temporary, path)
    };
    write().map_err(|e| {
        let _ = fs::remove_file(&temporary);
        format!("{path}: {e}")
    })
}

fn remove(path: &str) -> Result<(), String> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(format!("{path}: {e}")),
        _ => Ok(()),
    }
}

/// Put the dispatcher in place, where it is not as it should be. libvirt looks for hooks
/// only as it starts or reloads, so a new one comes with a reload.
fn install_dispatcher() -> Result<(), String> {
    if fs::read(DISPATCHER).is_ok_and(|d| d == DISPATCHER_SCRIPT.as_bytes()) {
        return Ok(());
    }
    let new = fs::symlink_metadata(DISPATCHER).is_err();
    make_folder("/etc/libvirt/hooks")?;
    make_folder("/etc/libvirt/hooks/qemu.d")?;
    write_executable(DISPATCHER, DISPATCHER_SCRIPT.as_bytes())?;
    if new {
        // Each daemon only if it runs; a stopped one finds the hook when it starts.
        for daemon in ["virtqemud.service", "libvirtd.service"] {
            let _ = Command::new("/usr/bin/systemctl")
                .args(["try-reload-or-restart", daemon])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_uuids_name_a_folder() {
        assert!(is_uuid("4f5b36fc-9b0b-4e0e-8c1b-9b2f2a7d8f01"));
        assert!(!is_uuid("4F5B36FC-9B0B-4E0E-8C1B-9B2F2A7D8F01"));
        assert!(!is_uuid("../../../../etc/passwd"));
        assert!(!is_uuid("4f5b36fc-9b0b-4e0e-8c1b-9b2f2a7d8f01/.."));
        assert!(!is_uuid(""));
    }

    #[test]
    fn scripts_have_to_say_what_runs_them() {
        let script = |s: &str| checked_script(s.as_bytes().to_vec());
        assert_eq!(script(" \n"), Ok(None));
        assert_eq!(
            script("#!/bin/sh\necho hi\n"),
            Ok(Some("#!/bin/sh\necho hi\n".to_owned()))
        );
        assert!(script("echo hi\n").is_err());
        assert!(script("#!/bin/sh\n\0").is_err());
        assert!(checked_script(vec![b'#', b'!', 0xff]).is_err());
        assert!(checked_script(vec![b'#'; MAX_SCRIPT as usize + 1]).is_err());
    }
}
