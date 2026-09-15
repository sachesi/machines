pub mod delete;
pub mod hardware;
pub mod machine;
pub mod networks;
pub mod new_machine;
pub mod storage;

use gettextrs::gettext;

use crate::adw::prelude::*;
use crate::{adw, glib, gtk};

/// A size in bytes, in GiB and the like, the units sizes are asked for in.
pub fn size(bytes: u64) -> String {
    glib::format_size_full(bytes, glib::FormatSizeFlags::IEC_UNITS).to_string()
}

/// A dialog of `page` with Cancel and a suggested `confirm` button in its header bar; the
/// button starts insensitive.
pub fn form(title: &str, confirm: &str, page: &adw::PreferencesPage) -> (adw::Dialog, gtk::Button) {
    let confirm = gtk::Button::builder()
        .label(confirm)
        .use_underline(true)
        .sensitive(false)
        .css_classes(["suggested-action"])
        .build();
    let cancel = gtk::Button::builder()
        .label(gettext("_Cancel"))
        .use_underline(true)
        .build();
    let header = adw::HeaderBar::builder()
        .show_start_title_buttons(false)
        .show_end_title_buttons(false)
        .build();
    header.pack_start(&cancel);
    header.pack_end(&confirm);
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(page));
    let dialog = adw::Dialog::builder()
        .title(title)
        .content_width(480)
        .child(&toolbar)
        .default_widget(&confirm)
        .build();
    cancel.connect_clicked(glib::clone!(
        #[weak]
        dialog,
        move |_| {
            dialog.close();
        }
    ));
    (dialog, confirm)
}

/// Ask before something that cannot be undone; resolves to whether to go ahead.
pub async fn confirm(
    parent: &impl IsA<gtk::Widget>,
    heading: &str,
    body: &str,
    action: &str,
) -> bool {
    let dialog = adw::AlertDialog::builder()
        .heading(heading)
        .body(body)
        .close_response("cancel")
        .default_response("cancel")
        .build();
    dialog.add_responses(&[("cancel", &gettext("_Cancel")), ("confirm", action)]);
    dialog.set_response_appearance("confirm", adw::ResponseAppearance::Destructive);
    dialog.choose_future(Some(parent)).await == "confirm"
}

/// A flat "+" button for the header of a group.
pub fn add_button(tooltip: &str) -> gtk::Button {
    gtk::Button::builder()
        .icon_name("list-add-symbolic")
        .tooltip_text(tooltip)
        .valign(gtk::Align::Center)
        .css_classes(["flat"])
        .build()
}

/// A flat button at the end of a row that removes what the row shows.
pub fn remove_button(tooltip: &str) -> gtk::Button {
    gtk::Button::builder()
        .icon_name("list-remove-symbolic")
        .tooltip_text(tooltip)
        .valign(gtk::Align::Center)
        .css_classes(["flat"])
        .build()
}
