//! The scripts libvirt runs as root before a machine starts and after it stops, which the
//! helper `machines-hooks` writes through pkexec, and anyone may read.

use std::path::PathBuf;

use gettextrs::gettext;

use crate::gio;

/// Where the helper keeps them, in a folder for each machine by UUID.
const SCRIPTS: &str = "/etc/machines/scripts";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// Before libvirt gives the machine anything of the host's; a failure keeps it from
    /// starting.
    Prepare,
    /// Once the machine has stopped and libvirt has given back what it took.
    Release,
}

impl Event {
    fn name(self) -> &'static str {
        match self {
            Self::Prepare => "prepare",
            Self::Release => "release",
        }
    }
}

/// The script for the machine `uuid` at `event`, if it has one.
pub fn script(uuid: &str, event: Event) -> Option<String> {
    std::fs::read_to_string(format!("{SCRIPTS}/{uuid}/{}", event.name())).ok()
}

/// The helper beside the app: in libexec where it is installed, else in the same folder,
/// as cargo builds it. pkexec matches the path whole, so it has no `..` in it.
fn helper() -> Option<PathBuf> {
    // Linux names the app's file so once it has been replaced, as by an update.
    let exe = std::env::current_exe().ok()?;
    let exe = exe
        .to_str()
        .and_then(|e| e.strip_suffix(" (deleted)"))
        .map_or(exe.clone(), PathBuf::from);
    let exe = exe.canonicalize().ok()?;
    let dir = exe.parent()?;
    [
        dir.join("../libexec/machines-hooks"),
        dir.join("machines-hooks"),
    ]
    .into_iter()
    .find_map(|p| p.canonicalize().ok())
}

/// Make `script` the one for the machine `uuid` at `event`, or with an empty one, remove
/// it. `Ok(false)` where the user did not authenticate.
pub async fn set_script(uuid: &str, event: Event, script: &str) -> Result<bool, String> {
    let helper = helper().ok_or_else(|| gettext("The helper machines-hooks is not installed"))?;
    let argv = [
        "pkexec".as_ref(),
        helper.as_os_str(),
        "set".as_ref(),
        uuid.as_ref(),
        event.name().as_ref(),
    ];
    let process = gio::Subprocess::newv(
        &argv,
        gio::SubprocessFlags::STDIN_PIPE | gio::SubprocessFlags::STDERR_PIPE,
    )
    .map_err(|e| e.to_string())?;
    let (_, stderr) = process
        .communicate_utf8_future(Some(script.to_owned()))
        .await
        .map_err(|e| e.to_string())?;
    match process.exit_status() {
        0 => Ok(true),
        // pkexec's own, for the authentication dialog dismissed.
        126 => Ok(false),
        _ => Err(stderr
            .map(|e| e.trim().trim_start_matches("machines-hooks: ").to_owned())
            .filter(|e| !e.is_empty())
            .unwrap_or_else(|| gettext("The script could not be saved"))),
    }
}
