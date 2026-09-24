//! "Storage": the connection's storage pools, each with a page of its volumes.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use gettextrs::gettext;

use crate::adw::prelude::*;
use crate::details::info_row;
use crate::dialogs::{self, add_button, hardware, size};
use crate::host_xml::{HostDisk, PoolSource};
use crate::hypervisor::{HostUse, Hypervisor, Pool, Result, Volume};
use crate::window::MachinesWindow;
use crate::{adw, glib, gtk};

const VOLUME_FORMATS: [&str; 2] = ["qcow2", "raw"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PoolKind {
    Dir,
    Nfs,
    Lvm,
    Iscsi,
}

impl PoolKind {
    fn label(self) -> String {
        match self {
            Self::Dir => gettext("Directory"),
            Self::Nfs => gettext("NFS Share"),
            Self::Lvm => gettext("LVM Volume Group"),
            Self::Iscsi => gettext("iSCSI Target"),
        }
    }

    fn description(self) -> String {
        match self {
            Self::Dir => gettext("The disk images in a directory, made if it is not there."),
            Self::Nfs => gettext(
                "The disk images in a directory another machine exports, mounted where \
                 libvirt keeps its images.",
            ),
            Self::Lvm => gettext(
                "The logical volumes of a volume group the host already has; each new \
                 volume is a logical volume.",
            ),
            Self::Iscsi => gettext(
                "The LUNs of an iSCSI target, which machines use as disks. New volumes \
                 cannot be made in it.",
            ),
        }
    }
}

/// The dialog's widgets are held weakly, so that the closures in them, which hold this,
/// do not keep them alive past the dialog.
struct Storage {
    win: MachinesWindow,
    dialog: glib::WeakRef<adw::PreferencesDialog>,
    page: glib::WeakRef<adw::PreferencesPage>,
    list: glib::WeakRef<adw::PreferencesGroup>,
    disks: glib::WeakRef<adw::PreferencesGroup>,
    /// The pool whose page is open, by UUID.
    open: RefCell<Option<(String, glib::WeakRef<adw::NavigationPage>)>>,
    names: RefCell<Vec<String>>,
    uploads: RefCell<Vec<Rc<Upload>>>,
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
        disks: glib::WeakRef::new(),
        open: RefCell::default(),
        names: RefCell::default(),
        uploads: RefCell::default(),
    });
    storage.reload();
    dialog.present(Some(win));
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
            if let Some(Ok((pools, disks))) =
                this.win.call(|hv| Ok((hv.pools(), hv.host_disks()))).await
            {
                this.show(pools);
                this.show_disks(disks);
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
                dialog.add_toast(dialogs::toast(&e));
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
                group.set_description(Some(&glib::markup_escape_text(&e)));
                Vec::new()
            }
        };
        if pools.is_empty() && group.description().is_none_or(|d| d.is_empty()) {
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
            let row = adw::ActionRow::builder().activatable(true).build();
            dialogs::set_plain_text(&row, &pool.name, &subtitle);
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

    /// The host's own disks, which a machine can have whole.
    fn show_disks(&self, disks: Result<Vec<(HostDisk, HostUse)>>) {
        let Some(page) = self.page.upgrade() else {
            return;
        };
        if let Some(old) = self.disks.upgrade() {
            page.remove(&old);
        }
        let group = adw::PreferencesGroup::builder()
            .title(gettext("Host Disks"))
            .build();
        match disks {
            Ok(disks) if disks.is_empty() => {
                group.set_description(Some(&gettext("No disks found")));
            }
            Ok(disks) => {
                group.set_description(Some(&gettext(
                    "Add one to a virtual machine as storage to give it the whole disk.",
                )));
                let machines = self.win.machine_infos();
                for (disk, host_use) in disks {
                    let mut subtitle = format!("{} · {}", size(disk.size), disk.path);
                    let users = hardware::disk_users(&disk, &machines);
                    if host_use == HostUse::InUse {
                        subtitle = format!("{subtitle}\n{}", gettext("The host uses it"));
                    }
                    if !users.is_empty() {
                        subtitle = format!(
                            "{subtitle}\n{}",
                            gettext("Used by {machines}").replace("{machines}", &users.join(", "))
                        );
                    }
                    let row = adw::ActionRow::builder().subtitle_selectable(true).build();
                    dialogs::set_plain_text(&row, &disk.name(), &subtitle);
                    group.add(&row);
                }
            }
            Err(e) => group.set_description(Some(&glib::markup_escape_text(&e))),
        }
        page.add(&group);
        self.disks.set(Some(&group));
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
        if let Some(source) = &pool.config.source {
            overview.add(&info_row(&gettext("Source"), source));
        }
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
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        if pool.holds_images() {
            let upload = gtk::Button::builder()
                .icon_name("document-send-symbolic")
                .tooltip_text(gettext("Upload a File"))
                .valign(gtk::Align::Center)
                .css_classes(["flat"])
                .build();
            upload.connect_clicked(glib::clone!(
                #[strong(rename_to = this)]
                self,
                #[strong]
                pool,
                move |_| this.upload(&pool)
            ));
            buttons.append(&upload);
        }
        if pool.makes_volumes() {
            let add = add_button(&gettext("New Volume"));
            add.connect_clicked(glib::clone!(
                #[strong(rename_to = this)]
                self,
                #[strong]
                pool,
                move |_| this.new_volume(&pool)
            ));
            buttons.append(&add);
        }
        group.set_header_suffix(Some(&buttons));
        let uploads: Vec<Rc<Upload>> = self
            .uploads
            .borrow()
            .iter()
            .filter(|u| u.pool == pool.uuid)
            .cloned()
            .collect();
        if pool.volumes.is_empty() && uploads.is_empty() {
            group.set_description(Some(&gettext("No volumes")));
        }
        for upload in &uploads {
            let bar = gtk::ProgressBar::builder()
                .show_text(true)
                .valign(gtk::Align::Center)
                .hexpand(true)
                .build();
            let row = adw::ActionRow::new();
            dialogs::set_plain_text(&row, &upload.name, &gettext("Uploading"));
            row.add_suffix(&bar);
            upload.bar.replace(bar.downgrade());
            upload.show_progress();
            group.add(&row);
        }
        let machines = self.win.machine_infos();
        for vol in pool
            .volumes
            .iter()
            .filter(|v| !uploads.iter().any(|u| u.name == v.name))
        {
            let users: Vec<(String, bool)> = machines
                .iter()
                .filter(|m| {
                    m.config.as_ref().is_some_and(|c| {
                        c.disks
                            .iter()
                            .any(|d| d.source.as_deref() == Some(vol.path.as_str()))
                    })
                })
                .map(|m| (m.name.clone(), m.state.is_active()))
                .collect();
            group.add(&self.volume_row(vol, users));
        }
        group
    }

    fn volume_row(self: &Rc<Self>, vol: &Volume, users: Vec<(String, bool)>) -> adw::ActionRow {
        let running = users.iter().any(|(_, active)| *active);
        let users: Vec<String> = users.into_iter().map(|(name, _)| name).collect();
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
        let row = adw::ActionRow::new();
        dialogs::set_plain_text(&row, &vol.name, &subtitle);
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
        let resize = gtk::Button::builder()
            .label(gettext("Resize…"))
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        resize.connect_clicked(glib::clone!(
            #[strong(rename_to = this)]
            self,
            #[strong]
            vol,
            move |_| this.resize_volume(&vol, running)
        ));
        row.add_suffix(&resize);
        row.add_suffix(&delete);
        row
    }

    fn new_pool(self: &Rc<Self>) {
        let Some(parent) = self.dialog.upgrade() else {
            return;
        };
        // Mounting, LVM and iSCSI need root, which only the system connection has.
        let kinds: &[PoolKind] = if self.win.is_session() {
            &[PoolKind::Dir]
        } else {
            &[PoolKind::Dir, PoolKind::Nfs, PoolKind::Lvm, PoolKind::Iscsi]
        };
        let name = adw::EntryRow::builder()
            .title(gettext("_Name"))
            .use_underline(true)
            .build();
        let labels: Vec<String> = kinds.iter().map(|k| k.label()).collect();
        let labels: Vec<&str> = labels.iter().map(String::as_str).collect();
        let kind_row = adw::ComboRow::builder()
            .title(gettext("_Type"))
            .use_underline(true)
            .model(&gtk::StringList::new(&labels))
            .visible(kinds.len() > 1)
            .build();
        let choose = gtk::Button::builder()
            .label(gettext("_Choose…"))
            .use_underline(true)
            .valign(gtk::Align::Center)
            .build();
        let folder_row = adw::ActionRow::builder()
            .use_markup(false)
            .title(gettext("Directory"))
            .subtitle(gettext("None chosen"))
            .activatable_widget(&choose)
            .build();
        folder_row.add_suffix(&choose);
        let host = adw::EntryRow::builder()
            .title(gettext("_Host"))
            .use_underline(true)
            .build();
        let source = adw::EntryRow::builder().use_underline(true).build();
        let group = adw::PreferencesGroup::new();
        group.add(&name);
        group.add(&kind_row);
        group.add(&folder_row);
        group.add(&host);
        group.add(&source);
        let page = adw::PreferencesPage::new();
        page.add(&group);
        let (dialog, create) =
            dialogs::form(&gettext("New Storage Pool"), &gettext("C_reate"), &page);
        // Tall enough for the type with the most rows, which rows shown later would
        // otherwise scroll out of.
        dialog.set_content_height(400);

        let folder: Rc<RefCell<Option<String>>> = Rc::default();
        let taken = self.names.borrow().clone();
        let kinds: Rc<[PoolKind]> = kinds.into();
        let request = Rc::new(glib::clone!(
            #[strong]
            kinds,
            #[weak]
            name,
            #[weak]
            kind_row,
            #[weak]
            host,
            #[weak]
            source,
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
                let filled = |row: &adw::EntryRow| {
                    Some(row.text().trim().to_owned()).filter(|t| !t.is_empty())
                };
                let pool = match kinds.get(kind_row.selected() as usize)? {
                    PoolKind::Dir => PoolSource::Dir(folder.borrow().clone()?),
                    PoolKind::Nfs => PoolSource::Nfs {
                        host: filled(&host)?,
                        export: filled(&source)?,
                        mount: String::new(),
                    },
                    PoolKind::Lvm => PoolSource::Lvm(filled(&source)?),
                    PoolKind::Iscsi => PoolSource::Iscsi {
                        host: filled(&host)?,
                        target: filled(&source)?,
                    },
                };
                Some((ok.then_some(text)?, pool))
            }
        ));
        let sync = Rc::new(glib::clone!(
            #[weak]
            kind_row,
            #[weak]
            folder_row,
            #[weak]
            host,
            #[weak]
            source,
            #[weak]
            group,
            #[weak]
            create,
            #[strong]
            request,
            #[strong]
            kinds,
            move || {
                let kind = kinds
                    .get(kind_row.selected() as usize)
                    .copied()
                    .unwrap_or(PoolKind::Dir);
                folder_row.set_visible(kind == PoolKind::Dir);
                host.set_visible(matches!(kind, PoolKind::Nfs | PoolKind::Iscsi));
                source.set_visible(kind != PoolKind::Dir);
                source.set_title(&match kind {
                    PoolKind::Nfs => gettext("_Export Path"),
                    PoolKind::Lvm => gettext("_Volume Group"),
                    PoolKind::Iscsi => gettext("_Target IQN"),
                    PoolKind::Dir => String::new(),
                });
                group.set_description(Some(&kind.description()));
                create.set_sensitive(request().is_some());
            }
        ));
        sync();
        kind_row.connect_selected_notify(glib::clone!(
            #[strong]
            sync,
            move |_| sync()
        ));
        for entry in [&name, &host, &source] {
            entry.connect_changed(glib::clone!(
                #[strong]
                sync,
                move |_| sync()
            ));
        }
        choose.connect_clicked(glib::clone!(
            #[weak]
            dialog,
            #[weak]
            folder_row,
            #[strong]
            folder,
            #[strong]
            sync,
            move |_| {
                glib::spawn_future_local(glib::clone!(
                    #[strong]
                    folder,
                    #[strong]
                    sync,
                    async move {
                        let title = gettext("Choose a Directory");
                        let Some(path) = dialogs::choose_on_host(&dialog, &title, None, true).await
                        else {
                            return;
                        };
                        folder_row.set_subtitle(&path);
                        folder.replace(Some(path));
                        sync();
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
                if let Some((name, source)) = request() {
                    dialog.close();
                    this.act(move |hv| {
                        let source = match source {
                            PoolSource::Nfs { host, export, .. } => PoolSource::Nfs {
                                host,
                                export,
                                mount: hv.nfs_mount_point(&name),
                            },
                            source => source,
                        };
                        hv.create_pool(&name, &source)
                    });
                }
            }
        ));
        dialog.present(Some(&parent));
    }

    fn new_volume(self: &Rc<Self>, pool: &Pool) {
        let Some(parent) = self.dialog.upgrade() else {
            return;
        };
        // A logical volume has no format, nor a file name to carry one.
        let logical = pool.config.kind == "logical";
        let name = adw::EntryRow::builder()
            .title(gettext("_Name"))
            .use_underline(true)
            .build();
        let format = adw::ComboRow::builder()
            .title(gettext("_Format"))
            .subtitle(gettext("A qcow2 image takes up only what the guest writes"))
            .use_underline(true)
            .model(&gtk::StringList::new(&VOLUME_FORMATS))
            .visible(!logical)
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
                let extension = (!logical).then(|| VOLUME_FORMATS[format.selected() as usize]);
                let full = match extension {
                    Some(extension) if !text.contains('.') => format!("{text}.{extension}"),
                    _ => text.clone(),
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
                if let Some((file, format)) = file_name() {
                    dialog.close();
                    let (uuid, gib) = (uuid.clone(), gib.value() as u64);
                    this.act(move |hv| hv.create_volume(&uuid, &file, gib, format));
                }
            }
        ));
        dialog.present(Some(&parent));
    }

    fn resize_volume(self: &Rc<Self>, vol: &Volume, running: bool) {
        let Some(parent) = self.dialog.upgrade() else {
            return;
        };
        let this = self.clone();
        let path = vol.path.clone();
        dialogs::resize(&parent, &vol.name, vol.capacity, running, move |bytes| {
            let path = path.clone();
            this.act(move |hv| hv.resize_volume(&path, bytes));
        });
    }

    /// Copy a file of the user's into the pool, with the progress in the pool's page.
    fn upload(self: &Rc<Self>, pool: &Pool) {
        let Some(parent) = self.dialog.upgrade() else {
            return;
        };
        let this = self.clone();
        let pool = pool.uuid.clone();
        glib::spawn_future_local(async move {
            let chooser = gtk::FileDialog::builder()
                .title(gettext("Upload a File"))
                .build();
            let window = parent.root().and_downcast::<gtk::Window>();
            let Ok(file) = chooser.open_future(window.as_ref()).await else {
                return;
            };
            let Some(path) = file.path() else {
                return;
            };
            let total = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            let upload = Rc::new(Upload {
                pool: pool.clone(),
                name: path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                total,
                sent: Arc::default(),
                bar: RefCell::default(),
            });
            this.uploads.borrow_mut().push(upload.clone());
            this.reload();
            let ticker = glib::timeout_add_local(
                Duration::from_millis(250),
                glib::clone!(
                    #[strong]
                    upload,
                    move || {
                        upload.show_progress();
                        glib::ControlFlow::Continue
                    }
                ),
            );
            let sent = upload.sent.clone();
            let result = this
                .win
                .call(move |hv| hv.upload_volume(&pool, &path, &sent))
                .await;
            ticker.remove();
            this.uploads
                .borrow_mut()
                .retain(|u| !Rc::ptr_eq(u, &upload));
            if let Some(Err(e)) = result
                && let Some(dialog) = this.dialog.upgrade()
            {
                dialog.add_toast(dialogs::toast(&e));
            }
            this.reload();
        });
    }
}

/// A file on its way into a pool.
struct Upload {
    pool: String,
    name: String,
    total: u64,
    sent: Arc<AtomicU64>,
    /// Its bar in the pool's page, while that is open.
    bar: RefCell<glib::WeakRef<gtk::ProgressBar>>,
}

impl Upload {
    fn show_progress(&self) {
        if let Some(bar) = self.bar.borrow().upgrade() {
            let sent = self.sent.load(Ordering::Relaxed);
            bar.set_fraction(sent as f64 / self.total.max(1) as f64);
            bar.set_text(Some(
                &gettext("{sent} of {total}")
                    .replace("{sent}", &size(sent))
                    .replace("{total}", &size(self.total)),
            ));
        }
    }
}
