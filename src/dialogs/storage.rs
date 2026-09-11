//! "Storage": the connection's storage pools, each with a page of its volumes.

use std::cell::RefCell;
use std::rc::Rc;

use gettextrs::gettext;

use crate::adw::prelude::*;
use crate::details::info_row;
use crate::dialogs::{self, add_button};
use crate::hypervisor::{Hypervisor, Pool, Result, Volume};
use crate::window::MachinesWindow;
use crate::{adw, glib, gtk};

const VOLUME_FORMATS: [&str; 2] = ["qcow2", "raw"];

/// The dialog's widgets are held weakly, so that the closures in them, which hold this,
/// do not keep them alive past the dialog.
struct Storage {
    win: MachinesWindow,
    dialog: glib::WeakRef<adw::PreferencesDialog>,
    page: glib::WeakRef<adw::PreferencesPage>,
    list: glib::WeakRef<adw::PreferencesGroup>,
    /// The pool whose page is open, by UUID.
    open: RefCell<Option<(String, glib::WeakRef<adw::NavigationPage>)>>,
    names: RefCell<Vec<String>>,
}

pub fn present(win: &MachinesWindow) {
    let dialog = adw::PreferencesDialog::builder()
        .title(gettext("Storage"))
        .content_height(620)
        .build();
    let page = adw::PreferencesPage::new();
    dialog.add(&page);
    let storage = Rc::new(Storage {
        win: win.clone(),
        dialog: dialog.downgrade(),
        page: page.downgrade(),
        list: glib::WeakRef::new(),
        open: RefCell::default(),
        names: RefCell::default(),
    });
    storage.reload();
    dialog.present(Some(win));
}

/// In GiB and the like, the units sizes are asked for in.
fn size(bytes: u64) -> String {
    glib::format_size_full(bytes, glib::FormatSizeFlags::IEC_UNITS).to_string()
}

fn kind(pool: &Pool) -> String {
    match pool.config.kind.as_str() {
        "dir" => gettext("Directory"),
        "fs" => gettext("File System"),
        "netfs" => gettext("Network File System"),
        "logical" => gettext("LVM Volume Group"),
        "disk" => gettext("Disk"),
        "iscsi" | "iscsi-direct" => "iSCSI".to_owned(),
        "zfs" => "ZFS".to_owned(),
        other => other.to_owned(),
    }
}

impl Storage {
    fn reload(self: &Rc<Self>) {
        let this = self.clone();
        glib::spawn_future_local(async move {
            if let Some(pools) = this.win.call(|hv| hv.pools()).await {
                this.show(pools);
            }
        });
    }

    /// Run `f` against the connection, then show what it left.
    fn act<F>(self: &Rc<Self>, f: F)
    where
        F: FnOnce(&Hypervisor) -> Result<()> + Send + 'static,
    {
        let this = self.clone();
        glib::spawn_future_local(async move {
            if let Some(Err(e)) = this.win.call(f).await
                && let Some(dialog) = this.dialog.upgrade()
            {
                dialog.add_toast(adw::Toast::new(&e));
            }
            this.reload();
        });
    }

    fn show(self: &Rc<Self>, pools: Result<Vec<Pool>>) {
        let (Some(dialog), Some(page)) = (self.dialog.upgrade(), self.page.upgrade()) else {
            return;
        };
        if let Some(old) = self.list.upgrade() {
            page.remove(&old);
        }
        let group = adw::PreferencesGroup::builder()
            .title(gettext("Storage Pools"))
            .build();
        let add = add_button(&gettext("New Storage Pool"));
        add.connect_clicked(glib::clone!(
            #[strong(rename_to = this)]
            self,
            move |_| this.new_pool()
        ));
        group.set_header_suffix(Some(&add));
        let pools = match pools {
            Ok(pools) => pools,
            Err(e) => {
                group.set_description(Some(&e));
                Vec::new()
            }
        };
        if pools.is_empty() && group.description().is_none() {
            group.set_description(Some(&gettext("No storage pools")));
        }
        for pool in &pools {
            let subtitle = match &pool.config.path {
                Some(path) if pool.active => gettext("{path} · {free} free")
                    .replace("{path}", path)
                    .replace("{free}", &size(pool.available)),
                Some(path) => gettext("{path} · Inactive").replace("{path}", path),
                None => kind(pool),
            };
            let row = adw::ActionRow::builder()
                .title(&pool.name)
                .subtitle(subtitle)
                .activatable(true)
                .build();
            row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
            row.connect_activated(glib::clone!(
                #[strong(rename_to = this)]
                self,
                #[strong]
                pool,
                move |_| this.open_pool(&pool)
            ));
            group.add(&row);
        }
        page.add(&group);
        self.list.set(Some(&group));
        self.names
            .replace(pools.iter().map(|p| p.name.clone()).collect());

        let open = self.open.borrow().clone();
        if let Some((uuid, sub)) = open
            && let Some(sub) = sub.upgrade()
        {
            match pools.iter().find(|p| p.uuid == uuid) {
                Some(pool) => sub.set_child(Some(&self.pool_page(pool))),
                None => {
                    dialog.pop_subpage();
                }
            }
        }
    }

    fn open_pool(self: &Rc<Self>, pool: &Pool) {
        let Some(dialog) = self.dialog.upgrade() else {
            return;
        };
        let sub = adw::NavigationPage::builder()
            .title(&pool.name)
            .child(&self.pool_page(pool))
            .build();
        sub.connect_hidden(glib::clone!(
            #[strong(rename_to = this)]
            self,
            move |_| {
                this.open.take();
            }
        ));
        self.open
            .replace(Some((pool.uuid.clone(), sub.downgrade())));
        dialog.push_subpage(&sub);
    }

    fn pool_page(self: &Rc<Self>, pool: &Pool) -> adw::ToolbarView {
        let page = adw::PreferencesPage::new();
        let overview = adw::PreferencesGroup::new();
        if let Some(path) = &pool.config.path {
            overview.add(&info_row(&gettext("Location"), path));
        }
        overview.add(&info_row(&gettext("Type"), &kind(pool)));
        if pool.active {
            overview.add(&info_row(
                &gettext("Free Space"),
                &gettext("{free} of {capacity}")
                    .replace("{free}", &size(pool.available))
                    .replace("{capacity}", &size(pool.capacity)),
            ));
        }
        let active = adw::SwitchRow::builder()
            .title(gettext("Active"))
            .active(pool.active)
            .build();
        let uuid = pool.uuid.clone();
        active.connect_active_notify(glib::clone!(
            #[strong(rename_to = this)]
            self,
            move |row| {
                let (uuid, on) = (uuid.clone(), row.is_active());
                this.act(move |hv| hv.set_pool_active(&uuid, on));
            }
        ));
        overview.add(&active);
        if pool.persistent {
            let autostart = adw::SwitchRow::builder()
                .title(gettext("Start With the Host"))
                .active(pool.autostart)
                .build();
            let uuid = pool.uuid.clone();
            autostart.connect_active_notify(glib::clone!(
                #[strong(rename_to = this)]
                self,
                move |row| {
                    let (uuid, on) = (uuid.clone(), row.is_active());
                    this.act(move |hv| hv.set_pool_autostart(&uuid, on));
                }
            ));
            overview.add(&autostart);
        }
        page.add(&overview);
        page.add(&self.volumes(pool));

        let remove = adw::ButtonRow::builder()
            .title(gettext("_Remove Pool"))
            .use_underline(true)
            .build();
        remove.add_css_class("destructive-action");
        remove.connect_activated(glib::clone!(
            #[strong(rename_to = this)]
            self,
            #[strong]
            pool,
            move |row| {
                let this = this.clone();
                let pool = pool.clone();
                let row = row.clone();
                glib::spawn_future_local(async move {
                    let heading = gettext("Remove “{name}”?").replace("{name}", &pool.name);
                    let body = gettext(
                        "Its virtual machines keep their disks, and the pool's directory \
                         and the files in it stay where they are.",
                    );
                    if dialogs::confirm(&row, &heading, &body, &gettext("_Remove")).await {
                        this.act(move |hv| hv.remove_pool(&pool.uuid));
                    }
                });
            }
        ));
        let danger = adw::PreferencesGroup::new();
        danger.add(&remove);
        page.add(&danger);

        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());
        toolbar.set_content(Some(&page));
        toolbar
    }

    fn volumes(self: &Rc<Self>, pool: &Pool) -> adw::PreferencesGroup {
        let group = adw::PreferencesGroup::builder()
            .title(gettext("Volumes"))
            .build();
        if !pool.active {
            group.set_description(Some(&gettext("Start the pool to see its volumes.")));
            return group;
        }
        let add = add_button(&gettext("New Volume"));
        add.connect_clicked(glib::clone!(
            #[strong(rename_to = this)]
            self,
            #[strong]
            pool,
            move |_| this.new_volume(&pool)
        ));
        group.set_header_suffix(Some(&add));
        if pool.volumes.is_empty() {
            group.set_description(Some(&gettext("No volumes")));
        }
        let machines = self.win.machine_infos();
        for vol in &pool.volumes {
            let users: Vec<String> = machines
                .iter()
                .filter(|m| {
                    m.config.as_ref().is_some_and(|c| {
                        c.disks
                            .iter()
                            .any(|d| d.source.as_deref() == Some(vol.path.as_str()))
                    })
                })
                .map(|m| m.name.clone())
                .collect();
            group.add(&self.volume_row(vol, users));
        }
        group
    }

    fn volume_row(self: &Rc<Self>, vol: &Volume, users: Vec<String>) -> adw::ActionRow {
        let mut subtitle = gettext("{capacity}, {used} used")
            .replace("{capacity}", &size(vol.capacity))
            .replace("{used}", &size(vol.allocation));
        if let Some(format) = &vol.format {
            subtitle = format!("{subtitle} · {format}");
        }
        if !users.is_empty() {
            subtitle = format!(
                "{subtitle}\n{}",
                gettext("Used by {machines}").replace("{machines}", &users.join(", "))
            );
        }
        let row = adw::ActionRow::builder()
            .title(&vol.name)
            .subtitle(subtitle)
            .build();
        let delete = gtk::Button::builder()
            .icon_name("user-trash-symbolic")
            .tooltip_text(gettext("Delete"))
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        delete.connect_clicked(glib::clone!(
            #[strong(rename_to = this)]
            self,
            #[strong]
            vol,
            move |button| {
                let this = this.clone();
                let vol = vol.clone();
                let button = button.clone();
                let users = users.clone();
                glib::spawn_future_local(async move {
                    let heading = gettext("Delete “{name}”?").replace("{name}", &vol.name);
                    let mut body = gettext("The file is deleted for good: {path}")
                        .replace("{path}", &vol.path);
                    if !users.is_empty() {
                        body = format!(
                            "{body}\n\n{}",
                            gettext("It is a disk of {machines}, which will not start without it.")
                                .replace("{machines}", &users.join(", "))
                        );
                    }
                    if dialogs::confirm(&button, &heading, &body, &gettext("_Delete")).await {
                        this.act(move |hv| hv.delete_volume(&vol.path));
                    }
                });
            }
        ));
        row.add_suffix(&delete);
        row
    }

    fn new_pool(self: &Rc<Self>) {
        let Some(parent) = self.dialog.upgrade() else {
            return;
        };
        let name = adw::EntryRow::builder()
            .title(gettext("_Name"))
            .use_underline(true)
            .build();
        let choose = gtk::Button::builder()
            .label(gettext("_Choose…"))
            .use_underline(true)
            .valign(gtk::Align::Center)
            .build();
        let folder_row = adw::ActionRow::builder()
            .title(gettext("Directory"))
            .subtitle(gettext("None chosen"))
            .activatable_widget(&choose)
            .build();
        folder_row.add_suffix(&choose);
        let group = adw::PreferencesGroup::builder()
            .description(gettext(
                "A pool of the disk images in a directory, made if it is not there.",
            ))
            .build();
        group.add(&name);
        group.add(&folder_row);
        let page = adw::PreferencesPage::new();
        page.add(&group);
        let (dialog, create) =
            dialogs::form(&gettext("New Storage Pool"), &gettext("C_reate"), &page);

        let folder: Rc<RefCell<Option<String>>> = Rc::default();
        let taken = self.names.borrow().clone();
        let valid = Rc::new(glib::clone!(
            #[weak]
            name,
            #[strong]
            folder,
            #[upgrade_or]
            None,
            move || {
                let text = name.text().trim().to_owned();
                let ok = !text.is_empty() && !text.contains('/') && !taken.contains(&text);
                name.remove_css_class("error");
                if !text.is_empty() && !ok {
                    name.add_css_class("error");
                }
                Some((ok.then_some(text)?, folder.borrow().clone()?))
            }
        ));
        name.connect_changed(glib::clone!(
            #[weak]
            create,
            #[strong]
            valid,
            move |_| create.set_sensitive(valid().is_some())
        ));
        choose.connect_clicked(glib::clone!(
            #[weak]
            dialog,
            #[weak]
            folder_row,
            #[weak]
            create,
            #[strong]
            folder,
            #[strong]
            valid,
            move |_| {
                glib::spawn_future_local(glib::clone!(
                    #[strong]
                    folder,
                    #[strong]
                    valid,
                    async move {
                        let chooser = gtk::FileDialog::builder()
                            .title(gettext("Choose a Directory"))
                            .build();
                        let window = dialog.root().and_downcast::<gtk::Window>();
                        let Ok(file) = chooser.select_folder_future(window.as_ref()).await else {
                            return;
                        };
                        let Some(path) = file.path() else {
                            return;
                        };
                        let path = path.to_string_lossy().into_owned();
                        folder_row.set_subtitle(&path);
                        folder.replace(Some(path));
                        create.set_sensitive(valid().is_some());
                    }
                ));
            }
        ));
        create.connect_clicked(glib::clone!(
            #[strong(rename_to = this)]
            self,
            #[weak]
            dialog,
            move |_| {
                if let Some((name, path)) = valid() {
                    dialog.close();
                    this.act(move |hv| hv.create_pool(&name, &path));
                }
            }
        ));
        dialog.present(Some(&parent));
    }

    fn new_volume(self: &Rc<Self>, pool: &Pool) {
        let Some(parent) = self.dialog.upgrade() else {
            return;
        };
        let name = adw::EntryRow::builder()
            .title(gettext("_Name"))
            .use_underline(true)
            .build();
        let format = adw::ComboRow::builder()
            .title(gettext("_Format"))
            .subtitle(gettext("A qcow2 image takes up only what the guest writes"))
            .use_underline(true)
            .model(&gtk::StringList::new(&VOLUME_FORMATS))
            .build();
        let gib = adw::SpinRow::builder()
            .title(gettext("_Size"))
            .subtitle(gettext("GiB"))
            .use_underline(true)
            .adjustment(&gtk::Adjustment::new(32.0, 1.0, 16384.0, 1.0, 16.0, 0.0))
            .build();
        let group = adw::PreferencesGroup::new();
        group.add(&name);
        group.add(&format);
        group.add(&gib);
        let page = adw::PreferencesPage::new();
        page.add(&group);
        let (dialog, create) = dialogs::form(&gettext("New Volume"), &gettext("C_reate"), &page);

        let taken: Vec<String> = pool.volumes.iter().map(|v| v.name.clone()).collect();
        // The format's extension goes on a name that has none.
        let file_name = Rc::new(glib::clone!(
            #[weak]
            name,
            #[weak]
            format,
            #[upgrade_or]
            None,
            move || {
                let text = name.text().trim().to_owned();
                let extension = VOLUME_FORMATS[format.selected() as usize];
                let full = if text.contains('.') {
                    text.clone()
                } else {
                    format!("{text}.{extension}")
                };
                let ok = !text.is_empty() && !text.contains('/') && !taken.contains(&full);
                name.remove_css_class("error");
                if !text.is_empty() && !ok {
                    name.add_css_class("error");
                }
                ok.then_some((full, extension))
            }
        ));
        name.connect_changed(glib::clone!(
            #[weak]
            create,
            #[strong]
            file_name,
            move |_| create.set_sensitive(file_name().is_some())
        ));
        format.connect_selected_notify(glib::clone!(
            #[weak]
            create,
            #[strong]
            file_name,
            move |_| create.set_sensitive(file_name().is_some())
        ));
        let uuid = pool.uuid.clone();
        create.connect_clicked(glib::clone!(
            #[strong(rename_to = this)]
            self,
            #[weak]
            dialog,
            #[weak]
            gib,
            move |_| {
                if let Some((file, extension)) = file_name() {
                    dialog.close();
                    let (uuid, gib) = (uuid.clone(), gib.value() as u64);
                    this.act(move |hv| hv.create_volume(&uuid, &file, gib, extension));
                }
            }
        ));
        dialog.present(Some(&parent));
    }
}
