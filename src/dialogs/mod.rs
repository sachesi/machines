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

/// A toast of `text` as it is; a toast would otherwise read it as markup, and show nothing
/// of an error with a `<` or `&` in it.
pub fn toast(text: &str) -> adw::Toast {
    adw::Toast::builder().title(text).use_markup(false).build()
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

/// Ask how far to grow the disk `name`, now `capacity` bytes, and hand the new size in
/// bytes to `grow`. `running` is whether a running guest has the disk.
pub fn resize(
    parent: &impl IsA<gtk::Widget>,
    name: &str,
    capacity: u64,
    running: bool,
    grow: impl Fn(u64) + 'static,
) {
    const GIB: f64 = (1u64 << 30) as f64;
    let current = capacity as f64 / GIB;
    let gib = adw::SpinRow::builder()
        .title(gettext("_Size"))
        .subtitle(gettext("GiB"))
        .use_underline(true)
        .digits(1)
        .adjustment(&gtk::Adjustment::new(
            current.ceil(),
            current.ceil(),
            16384.0,
            1.0,
            16.0,
            0.0,
        ))
        .build();
    let group = adw::PreferencesGroup::builder()
        .description(if running {
            gettext(
                "The running guest sees the disk grow at once. Its partitions and file \
                 systems stay the size they are until they are grown in the guest.",
            )
        } else {
            gettext(
                "The disk only grows. Its partitions and file systems stay the size they \
                 are until they are grown in the guest.",
            )
        })
        .build();
    group.add(&gib);
    let page = adw::PreferencesPage::new();
    page.add(&group);
    let heading = gettext("Resize “{name}”").replace("{name}", name);
    let (dialog, resize) = form(&heading, &gettext("_Resize"), &page);
    let sync = move |row: &adw::SpinRow, button: &gtk::Button| {
        button.set_sensitive((row.value() * GIB) as u64 > capacity);
    };
    gib.connect_value_notify(glib::clone!(
        #[weak]
        resize,
        move |row| sync(row, &resize)
    ));
    resize.connect_clicked(glib::clone!(
        #[weak]
        dialog,
        #[weak]
        gib,
        move |_| {
            dialog.close();
            grow((gib.value() * GIB) as u64);
        }
    ));
    dialog.present(Some(parent));
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
