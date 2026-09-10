//! "Delete Virtual Machine" confirmation.

use gettextrs::gettext;

use crate::adw::prelude::*;
use crate::{adw, gtk};

/// Resolves to whether the disk images go too, or None if cancelled.
pub async fn confirm(parent: &impl IsA<gtk::Widget>, name: &str, files: &[String]) -> Option<bool> {
    let dialog = adw::AlertDialog::builder()
        .heading(gettext("Delete “{name}”?").replace("{name}", name))
        .body(gettext(
            "The virtual machine is removed, with its firmware variables and snapshots.",
        ))
        .close_response("cancel")
        .default_response("cancel")
        .build();
    dialog.add_responses(&[
        ("cancel", &gettext("_Cancel")),
        ("delete", &gettext("_Delete")),
    ]);
    dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);

    let check = gtk::CheckButton::builder()
        .label(gettext("Also delete its disk images"))
        .active(true)
        .build();
    if !files.is_empty() {
        let list = gtk::Label::builder()
            .label(files.join("\n"))
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .xalign(0.0)
            .selectable(true)
            .css_classes(["caption", "dim-label"])
            .margin_start(28)
            .build();
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .build();
        content.append(&check);
        content.append(&list);
        dialog.set_extra_child(Some(&content));
    }
    if dialog.choose_future(Some(parent)).await != "delete" {
        return None;
    }
    Some(!files.is_empty() && check.is_active())
}
