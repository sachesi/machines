mod application;
mod config;
mod console;
mod details;
mod dialogs;
mod domain_xml;
mod hypervisor;
mod keymap;
mod machine;
mod machine_view;
mod prefs;
mod window;

pub use adw::{gdk, gio, glib, gtk};
pub use libadwaita as adw;

use application::MachinesApplication;
use gio::prelude::*;

fn main() -> glib::ExitCode {
    // SAFETY: the first thing the program does; no thread has been started.
    unsafe { gettextrs::setlocale(gettextrs::LocaleCategory::LcAll, "") };
    gettextrs::bindtextdomain(config::GETTEXT_PACKAGE, config::LOCALEDIR).ok();
    gettextrs::bind_textdomain_codeset(config::GETTEXT_PACKAGE, "UTF-8").ok();
    gettextrs::textdomain(config::GETTEXT_PACKAGE).ok();

    gio::resources_register_include!("machines.gresource").expect("register resources");
    glib::set_application_name("Machines");
    // libvirt prints every error to stderr by default; they reach the user as toasts.
    virt::error::clear_error_callback();
    MachinesApplication::new().run()
}
