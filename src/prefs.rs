use crate::gio;

thread_local! {
    static SETTINGS: gio::Settings = gio::Settings::new(crate::config::APP_ID);
}

pub fn settings() -> gio::Settings {
    SETTINGS.with(|s| s.clone())
}

pub const SYSTEM_URI: &str = "qemu:///system";
pub const SESSION_URI: &str = "qemu:///session";
