pub mod delete;
pub mod hardware;
pub mod machine;
pub mod networks;
pub mod new_machine;
pub mod storage;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use gettextrs::gettext;

use crate::adw::prelude::*;
use crate::window::MachinesWindow;
use crate::{adw, gio, glib, gtk};

/// A size in bytes, in GiB and the like, the units sizes are asked for in.
pub fn size(bytes: u64) -> String {
    glib::format_size_full(bytes, glib::FormatSizeFlags::IEC_UNITS).to_string()
}

/// A toast of `text` as it is; a toast would otherwise read it as markup, and show nothing
/// of an error with a `<` or `&` in it.
pub fn toast(text: &str) -> adw::Toast {
    adw::Toast::builder().title(text).use_markup(false).build()
}

/// Show `title` and `subtitle` in `row` as they are. Rows read their text as markup, and
/// one built with both markup off and text still parses the text first.
pub fn set_plain_text(row: &impl IsA<adw::ActionRow>, title: &str, subtitle: &str) {
    let row = row.upcast_ref::<adw::ActionRow>();
    row.set_use_markup(false);
    PreferencesRowExt::set_title(row, title);
    row.set_subtitle(subtitle);
}

/// A file, or with `folder` a folder, of the host libvirt runs on, which on this computer
/// the file chooser picks, and on another is typed in.
pub async fn choose_on_host(
    parent: &impl IsA<gtk::Widget>,
    title: &str,
    filter: Option<&gtk::FileFilter>,
    folder: bool,
) -> Option<String> {
    let window = parent.root().and_downcast::<gtk::Window>();
    let local = window
        .as_ref()
        .and_then(|w| w.downcast_ref::<MachinesWindow>())
        .is_none_or(|w| w.host().local);
    if !local {
        return ask_host_path(parent, title, folder).await;
    }
    let dialog = gtk::FileDialog::builder().title(title).build();
    if let Some(filter) = filter {
        let filters = gio::ListStore::new::<gtk::FileFilter>();
        filters.append(filter);
        dialog.set_filters(Some(&filters));
    }
    let file = if folder {
        dialog.select_folder_future(window.as_ref()).await
    } else {
        dialog.open_future(window.as_ref()).await
    };
    file.ok()?.path().map(|p| p.to_string_lossy().into_owned())
}

/// A path typed in, of a file or folder on the host, which is another computer.
async fn ask_host_path(
    parent: &impl IsA<gtk::Widget>,
    title: &str,
    folder: bool,
) -> Option<String> {
    let dialog = adw::AlertDialog::builder()
        .heading(title)
        .body(if folder {
            gettext(
                "The virtual machines run on another computer. Type the path of the folder there.",
            )
        } else {
            gettext(
                "The virtual machines run on another computer. Type the path of the file there.",
            )
        })
        .close_response("cancel")
        .default_response("choose")
        .build();
    dialog.add_responses(&[
        ("cancel", &gettext("_Cancel")),
        ("choose", &gettext("_Choose")),
    ]);
    dialog.set_response_appearance("choose", adw::ResponseAppearance::Suggested);
    dialog.set_response_enabled("choose", false);
    let entry = adw::EntryRow::builder()
        .title(gettext("Path"))
        .activates_default(true)
        .build();
    entry.connect_changed(glib::clone!(
        #[weak]
        dialog,
        move |entry| dialog.set_response_enabled("choose", entry.text().starts_with('/'))
    ));
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    list.append(&entry);
    dialog.set_extra_child(Some(&list));
    dialog.set_focus(Some(&entry));
    (dialog.choose_future(Some(parent)).await == "choose").then(|| entry.text().to_string())
}

/// The first folder on the way to `path` that other users cannot enter, and so neither can
/// QEMU where it runs as a user of its own.
fn closed_to_qemu(path: &Path) -> Option<PathBuf> {
    path.ancestors().skip(1).find_map(|dir| {
        let mode = std::fs::metadata(dir).ok()?.permissions().mode();
        (mode & 0o001 == 0).then(|| dir.to_owned())
    })
}

/// What to tell of `path` before QEMU, running as a user of its own, fails to open it. The
/// folders on the way are looked at off the main loop, as a network share among them can be
/// slow to answer.
pub async fn qemu_access_warning(path: PathBuf) -> Option<String> {
    let dir = gio::spawn_blocking(move || closed_to_qemu(&path))
        .await
        .ok()
        .flatten()?;
    Some(
        gettext(
            "QEMU runs as a user of its own, which cannot open files in {folder}. Move the \
             file elsewhere, such as /var/lib/libvirt/images, or let that user into the folder.",
        )
        .replace("{folder}", &dir.to_string_lossy()),
    )
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_private_folder_on_the_way_keeps_qemu_out() {
        let base = std::env::temp_dir().join(format!("machines-{}", std::process::id()));
        let (closed, open) = (base.join("closed"), base.join("open"));
        std::fs::create_dir_all(&closed).unwrap();
        std::fs::create_dir_all(&open).unwrap();
        let mode = |dir: &Path, mode| {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode)).unwrap()
        };
        mode(&base, 0o755);
        mode(&closed, 0o700);
        mode(&open, 0o711);
        assert_eq!(closed_to_qemu(&closed.join("a.iso")), Some(closed.clone()));
        assert!(closed_to_qemu(&open.join("a.iso")).is_none_or(|d| !d.starts_with(&base)));
        let warning = glib::MainContext::new().block_on(qemu_access_warning(closed.join("a.iso")));
        assert!(warning.is_some_and(|w| w.contains(&*closed.to_string_lossy())));
        std::fs::remove_dir_all(&base).unwrap();
    }
}
