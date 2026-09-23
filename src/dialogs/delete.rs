//! "Delete Virtual Machine" confirmation.

use gettextrs::gettext;

use crate::adw::prelude::*;
use crate::{adw, gtk};

/// Resolves to the disk images to delete with the machine, or None if cancelled.
///
/// `images` pairs each image only this machine writes to with whether it is ticked at
/// first; `kept` says whether it has others, which stay.
pub async fn confirm(
    parent: &impl IsA<gtk::Widget>,
    name: &str,
    running: bool,
    images: &[(String, bool)],
    kept: bool,
) -> Option<Vec<String>> {
    let mut body =
        gettext("The virtual machine is removed, with its firmware variables and snapshots.");
    if running {
        body = format!(
            "{body} {}",
            gettext("It is forced off first, and work not saved in it is lost.")
        );
    }
    let dialog = adw::AlertDialog::builder()
        .heading(gettext("Delete “{name}”?").replace("{name}", name))
        .body(body)
        .close_response("cancel")
        .default_response("cancel")
        .build();
    dialog.add_responses(&[
        ("cancel", &gettext("_Cancel")),
        ("delete", &gettext("_Delete")),
    ]);
    dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .build();
    let checks: Vec<(String, gtk::CheckButton)> = images
        .iter()
        .map(|(path, ticked)| {
            let label = gtk::Label::builder()
                .label(path)
                .wrap(true)
                .wrap_mode(gtk::pango::WrapMode::WordChar)
                .xalign(0.0)
                .build();
            let check = gtk::CheckButton::builder()
                .child(&label)
                .active(*ticked)
                .build();
            (path.clone(), check)
        })
        .collect();
    if !checks.is_empty() {
        content.append(
            &gtk::Label::builder()
                .label(gettext("Delete its disk images too:"))
                .xalign(0.0)
                .css_classes(["heading"])
                .build(),
        );
        for (_, check) in &checks {
            content.append(check);
        }
    }
    if kept {
        content.append(
            &gtk::Label::builder()
                .label(gettext(
                    "Disk images other virtual machines use, and the ones it only reads, stay.",
                ))
                .wrap(true)
                .xalign(0.0)
                .css_classes(["caption", "dim-label"])
                .build(),
        );
    }
    if content.first_child().is_some() {
        dialog.set_extra_child(Some(&content));
    }
    if dialog.choose_future(Some(parent)).await != "delete" {
        return None;
    }
    Some(
        checks
            .into_iter()
            .filter(|(_, check)| check.is_active())
            .map(|(path, _)| path)
            .collect(),
    )
}
