//! The main window: the machines of one libvirt connection in the sidebar, the selected
//! one in the content pane.
//!
//! The machines are listed again whenever libvirt tells of a change to one, and every so
//! often for what it tells nothing of.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::Arc;

use gettextrs::gettext;

use crate::adw::prelude::*;
use crate::adw::subclass::prelude::*;
use crate::hypervisor::{Event, Host, Hypervisor, MachineInfo, Result};
use crate::machine::Machine;
use crate::machine_view::MachineView;
use crate::{adw, dialogs, gio, glib, gtk, prefs};

/// How often the machines are listed again when libvirt does not tell of their changes.
const POLL_SECONDS: u32 = 2;
/// How often they are when it does, for what it has no event for, such as a snapshot taken
/// elsewhere.
const RESCAN_SECONDS: u32 = 30;

mod imp {
    use super::*;

    #[derive(gtk::CompositeTemplate)]
    #[template(resource = "/io/github/sachesi/machines/ui/window.ui")]
    pub struct MachinesWindow {
        #[template_child]
        pub toasts: TemplateChild<adw::ToastOverlay>,
        #[template_child]
        pub split_view: TemplateChild<adw::NavigationSplitView>,
        #[template_child]
        pub connection_title: TemplateChild<adw::WindowTitle>,
        #[template_child]
        pub sidebar_stack: TemplateChild<gtk::Stack>,
        #[template_child]
        pub machine_list: TemplateChild<gtk::ListView>,
        #[template_child]
        pub error_page: TemplateChild<adw::StatusPage>,
        #[template_child]
        pub session_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub content_page: TemplateChild<adw::NavigationPage>,
        #[template_child]
        pub content_stack: TemplateChild<gtk::Stack>,
        #[template_child]
        pub machine_view: TemplateChild<MachineView>,

        pub store: gio::ListStore,
        pub selection: gtk::SingleSelection,
        pub settings: gio::Settings,
        pub(super) hypervisor: RefCell<Option<Arc<Hypervisor>>>,
        pub(super) host: Cell<Host>,
        /// Bumped on every (re)connection, so answers from an older one are dropped.
        pub(super) connection: Cell<u64>,
        pub(super) listing: Cell<bool>,
        /// Whether something changed while a listing was under way, so it is out of date.
        pub(super) stale: Cell<bool>,
        /// A machine to select once a listing brings it, after it was just created.
        pub(super) pending_select: RefCell<Option<String>>,
        pub(super) poll: RefCell<Option<glib::SourceId>>,
        /// The sidebar's collapsed state from before fullscreen, to restore after.
        pub(super) collapsed_before_fullscreen: Cell<Option<bool>>,
    }

    impl Default for MachinesWindow {
        fn default() -> Self {
            let store = gio::ListStore::new::<Machine>();
            let sorter = gtk::StringSorter::new(Some(gtk::PropertyExpression::new(
                Machine::static_type(),
                gtk::Expression::NONE,
                "name",
            )));
            let sorted = gtk::SortListModel::new(Some(store.clone()), Some(sorter));
            Self {
                toasts: Default::default(),
                split_view: Default::default(),
                connection_title: Default::default(),
                sidebar_stack: Default::default(),
                machine_list: Default::default(),
                error_page: Default::default(),
                session_button: Default::default(),
                content_page: Default::default(),
                content_stack: Default::default(),
                machine_view: Default::default(),
                store,
                selection: gtk::SingleSelection::new(Some(sorted)),
                settings: prefs::settings(),
                hypervisor: Default::default(),
                host: Default::default(),
                connection: Default::default(),
                listing: Default::default(),
                stale: Default::default(),
                pending_select: Default::default(),
                poll: Default::default(),
                collapsed_before_fullscreen: Default::default(),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for MachinesWindow {
        const NAME: &'static str = "MachinesWindow";
        type Type = super::MachinesWindow;
        type ParentType = adw::ApplicationWindow;

        fn class_init(klass: &mut Self::Class) {
            MachineView::ensure_type();
            klass.bind_template();
            klass.install_action("win.new-machine", None, |win, _, _| win.new_machine());
            klass.install_action("win.reconnect", None, |win, _, _| win.connect());
            klass.install_action("win.storage", None, |win, _, _| {
                dialogs::storage::present(win);
            });
            klass.install_action("win.networks", None, |win, _, _| {
                dialogs::networks::present(win);
            });
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for MachinesWindow {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            let (w, h): (i32, i32) = self.settings.get("window-size");
            obj.set_default_size(w, h);
            if self.settings.boolean("window-maximized") {
                obj.maximize();
            }

            self.machine_list.set_model(Some(&self.selection));
            self.machine_list.set_factory(Some(&row_factory()));
            self.machine_list.connect_activate(glib::clone!(
                #[weak(rename_to = win)]
                obj,
                move |_, _| win.imp().split_view.set_show_content(true)
            ));
            self.selection.connect_selected_item_notify(glib::clone!(
                #[weak(rename_to = win)]
                obj,
                move |_| win.show_selected()
            ));
            self.settings.connect_changed(
                Some("connection-uri"),
                glib::clone!(
                    #[weak(rename_to = win)]
                    obj,
                    move |_, _| win.connect()
                ),
            );
            obj.connect_fullscreened_notify(|win| win.follow_fullscreen());
            obj.connect();
        }

        fn dispose(&self) {
            if let Some(poll) = self.poll.take() {
                poll.remove();
            }
        }
    }

    impl WidgetImpl for MachinesWindow {}

    impl WindowImpl for MachinesWindow {
        fn close_request(&self) -> glib::Propagation {
            let obj = self.obj();
            self.machine_view.attach_console();
            if !obj.is_fullscreen() {
                let (w, h) = obj.default_size();
                let _ = self.settings.set("window-size", (w, h));
                let _ = self
                    .settings
                    .set_boolean("window-maximized", obj.is_maximized());
            }
            self.parent_close_request()
        }
    }

    impl ApplicationWindowImpl for MachinesWindow {}
    impl AdwApplicationWindowImpl for MachinesWindow {}
}

glib::wrapper! {
    pub struct MachinesWindow(ObjectSubclass<imp::MachinesWindow>)
        @extends adw::ApplicationWindow, gtk::ApplicationWindow, gtk::Window, gtk::Widget,
        @implements gio::ActionGroup, gio::ActionMap, gtk::Accessible, gtk::Buildable,
                    gtk::ConstraintTarget, gtk::Native, gtk::Root, gtk::ShortcutManager;
}

/// A sidebar row: the machine's name over its state.
fn row_factory() -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().expect("a ListItem");
        let name = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        let status = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["caption", "dim-label"])
            .build();
        let labels = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .valign(gtk::Align::Center)
            .build();
        labels.append(&name);
        labels.append(&status);
        let icon = gtk::Image::from_icon_name("computer-symbolic");
        let row = gtk::Box::builder()
            .spacing(12)
            .margin_top(6)
            .margin_bottom(6)
            .build();
        row.append(&icon);
        row.append(&labels);
        item.set_child(Some(&row));

        let machine = item.property_expression("item");
        machine
            .chain_property::<Machine>("name")
            .bind(&name, "label", gtk::Widget::NONE);
        machine
            .chain_property::<Machine>("status")
            .bind(&status, "label", gtk::Widget::NONE);
        machine
            .chain_property::<Machine>("state")
            .chain_closure::<bool>(glib::closure!(
                |_: Option<glib::Object>, state: crate::hypervisor::MachineState| {
                    state.is_active()
                }
            ))
            .bind(&icon, "sensitive", gtk::Widget::NONE);
    });
    factory
}

impl MachinesWindow {
    pub fn new(app: &impl IsA<gtk::Application>) -> Self {
        glib::Object::builder().property("application", app).build()
    }

    pub fn toast(&self, text: &str) {
        self.imp().toasts.add_toast(dialogs::toast(text));
    }

    pub fn host(&self) -> Host {
        self.imp().host.get()
    }

    pub fn is_session(&self) -> bool {
        self.imp()
            .hypervisor
            .borrow()
            .as_ref()
            .is_some_and(|hv| hv.is_session())
    }

    pub fn machine_names(&self) -> Vec<String> {
        self.imp()
            .store
            .iter::<Machine>()
            .flatten()
            .map(|m| m.name())
            .collect()
    }

    /// Select the machine `uuid` once a listing has it.
    pub fn select_when_listed(&self, uuid: &str) {
        self.imp().pending_select.replace(Some(uuid.to_owned()));
        self.refresh();
    }

    pub fn machine_infos(&self) -> Vec<MachineInfo> {
        self.imp()
            .store
            .iter::<Machine>()
            .flatten()
            .filter_map(|m| m.info())
            .collect()
    }

    /// The actions that need a connection to act on.
    fn set_connected(&self, connected: bool) {
        for action in ["win.new-machine", "win.storage", "win.networks"] {
            self.action_set_enabled(action, connected);
        }
    }

    /// Run `f` against the connection off the main loop. `None` when there is no
    /// connection, or it changed while `f` ran.
    pub async fn call<T, F>(&self, f: F) -> Option<Result<T>>
    where
        T: Send + 'static,
        F: FnOnce(&Hypervisor) -> Result<T> + Send + 'static,
    {
        let imp = self.imp();
        let hv = imp.hypervisor.borrow().clone()?;
        let connection = imp.connection.get();
        let result = gio::spawn_blocking(move || f(&hv)).await.ok()?;
        (imp.connection.get() == connection).then_some(result)
    }

    /// Run `f` against the connection, show its error if it fails, and list the machines
    /// again after, for whatever it changed.
    pub fn run<F>(&self, f: F)
    where
        F: FnOnce(&Hypervisor) -> Result<()> + Send + 'static,
    {
        glib::spawn_future_local(glib::clone!(
            #[weak(rename_to = win)]
            self,
            async move {
                if let Some(Err(e)) = win.call(f).await {
                    win.toast(&e);
                }
                win.refresh();
            }
        ));
    }

    /// List the machines again every `seconds`.
    fn poll_every(&self, seconds: u32) {
        let poll = glib::timeout_add_seconds_local(
            seconds,
            glib::clone!(
                #[weak(rename_to = win)]
                self,
                #[upgrade_or]
                glib::ControlFlow::Break,
                move || {
                    win.refresh();
                    glib::ControlFlow::Continue
                }
            ),
        );
        if let Some(old) = self.imp().poll.replace(Some(poll)) {
            old.remove();
        }
    }

    fn follow_event(&self, connection: u64, event: Event) {
        if self.imp().connection.get() != connection {
            return;
        }
        match event {
            Event::Changed => self.refresh(),
            Event::Closed => {
                self.imp().hypervisor.replace(None);
                self.set_connected(false);
                self.show_error(&gettext("The connection to libvirt was lost"));
            }
            Event::DeviceAdded(name) => self.replug_usb(name),
        }
    }

    /// Give a USB device plugged in again back to the running machine that had it.
    fn replug_usb(&self, name: String) {
        glib::spawn_future_local(glib::clone!(
            #[weak(rename_to = win)]
            self,
            async move {
                match win.call(move |hv| hv.replug_usb(&name)).await {
                    Some(Ok(Some((device, machine)))) => win.toast(
                        &gettext("{device} went back to {machine}")
                            .replace("{device}", &dialogs::hardware::device_title(&device))
                            .replace("{machine}", &machine),
                    ),
                    Some(Err(e)) => win.toast(&e),
                    _ => {}
                }
            }
        ));
    }

    /// (Re)open the connection the settings name.
    pub fn connect(&self) {
        let imp = self.imp();
        let uri = imp.settings.string("connection-uri").to_string();
        let connection = imp.connection.get() + 1;
        imp.connection.set(connection);
        imp.hypervisor.replace(None);
        imp.store.remove_all();
        imp.connection_title.set_subtitle(&connection_label(&uri));
        imp.sidebar_stack.set_visible_child_name("loading");
        self.set_connected(false);
        let weak = glib::SendWeakRef::from(self.downgrade());
        let notify = move |event| {
            let weak = weak.clone();
            // Not right away: libvirt calls this holding the connection's locks, which
            // dropping the connection, as a closed one is, would take again.
            glib::idle_add_once(move || {
                if let Some(win) = weak.upgrade() {
                    win.follow_event(connection, event);
                }
            });
        };
        glib::spawn_future_local(glib::clone!(
            #[weak(rename_to = win)]
            self,
            async move {
                let opened = gio::spawn_blocking(move || {
                    Hypervisor::open(&uri).map(|hv| {
                        let host = hv.host();
                        let watched = hv.watch(notify).is_ok();
                        (hv, host, watched)
                    })
                })
                .await;
                let imp = win.imp();
                if imp.connection.get() != connection {
                    return;
                }
                match opened {
                    Ok(Ok((hv, host, watched))) => {
                        imp.hypervisor.replace(Some(Arc::new(hv)));
                        imp.host.set(host);
                        win.poll_every(if watched {
                            RESCAN_SECONDS
                        } else {
                            POLL_SECONDS
                        });
                        win.set_connected(true);
                        win.refresh();
                    }
                    Ok(Err(e)) => win.show_error(&e),
                    Err(_) => win.show_error(&gettext("The connection attempt failed")),
                }
            }
        ));
    }

    fn show_error(&self, message: &str) {
        let imp = self.imp();
        imp.error_page
            .set_description(Some(&glib::markup_escape_text(message)));
        imp.session_button
            .set_visible(imp.settings.string("connection-uri") != prefs::SESSION_URI);
        imp.sidebar_stack.set_visible_child_name("error");
        imp.content_stack.set_visible_child_name("none");
        imp.machine_view.set_machine(None);
    }

    /// List the machines again, unless a listing is already under way.
    pub fn refresh(&self) {
        let imp = self.imp();
        if imp.hypervisor.borrow().is_none() {
            return;
        }
        if imp.listing.get() {
            imp.stale.set(true);
            return;
        }
        imp.listing.set(true);
        glib::spawn_future_local(glib::clone!(
            #[weak(rename_to = win)]
            self,
            async move {
                let listed = win.call(|hv| hv.machines()).await;
                let imp = win.imp();
                imp.listing.set(false);
                match listed {
                    Some(Ok(machines)) => win.apply_listing(machines),
                    Some(Err(e)) => {
                        imp.hypervisor.replace(None);
                        win.show_error(&e);
                    }
                    None => {}
                }
                if imp.stale.take() {
                    win.refresh();
                }
            }
        ));
    }

    fn apply_listing(&self, machines: Vec<MachineInfo>) {
        let imp = self.imp();
        let mut listed: HashMap<String, MachineInfo> =
            machines.into_iter().map(|m| (m.uuid.clone(), m)).collect();
        let mut i = 0;
        while let Some(machine) = imp.store.item(i).and_downcast::<Machine>() {
            match listed.remove(&machine.uuid()) {
                Some(info) => {
                    machine.update(info);
                    i += 1;
                }
                None => imp.store.remove(i),
            }
        }
        let added: Vec<Machine> = listed.into_values().map(Machine::new).collect();
        imp.store.extend_from_slice(&added);

        if let Some(uuid) = imp.pending_select.take()
            && !self.select(&uuid)
        {
            imp.pending_select.replace(Some(uuid));
        }
        let empty = imp.store.n_items() == 0;
        imp.sidebar_stack
            .set_visible_child_name(if empty { "empty" } else { "list" });
        if empty {
            imp.content_stack.set_visible_child_name("none");
        }
    }

    /// Select the machine `uuid`; false if the list does not have it (yet).
    fn select(&self, uuid: &str) -> bool {
        let imp = self.imp();
        let model = imp.selection.model().expect("a model");
        let found = (0..model.n_items()).find(|&i| {
            model
                .item(i)
                .and_downcast::<Machine>()
                .is_some_and(|m| m.uuid() == uuid)
        });
        if let Some(i) = found {
            imp.selection.set_selected(i);
            imp.split_view.set_show_content(true);
        }
        found.is_some()
    }

    fn show_selected(&self) {
        let imp = self.imp();
        let machine = imp.selection.selected_item().and_downcast::<Machine>();
        imp.machine_view.set_machine(machine.as_ref());
        match machine {
            Some(machine) => {
                imp.content_page.set_title(&machine.name());
                imp.content_stack.set_visible_child_name("machine");
            }
            None => {
                imp.content_page.set_title(&gettext("Virtual Machine"));
                imp.content_stack.set_visible_child_name("none");
            }
        }
    }

    fn new_machine(&self) {
        dialogs::new_machine::present(self, move |win, request| {
            win.toast(&gettext("Creating “{name}”…").replace("{name}", &request.name));
            glib::spawn_future_local(glib::clone!(
                #[weak]
                win,
                async move {
                    let created = win.call(move |hv| Ok(hv.create(&request))).await;
                    let uuid = match created {
                        Some(Ok(Ok(uuid))) => Some(uuid),
                        Some(Ok(Err((uuid, e)))) => {
                            win.toast(&e);
                            uuid
                        }
                        _ => None,
                    };
                    if let Some(uuid) = uuid {
                        win.imp().pending_select.replace(Some(uuid));
                        win.imp().machine_view.show_console();
                    }
                    win.refresh();
                }
            ));
        });
    }

    /// Fullscreen is for the console: the sidebar folds away while it lasts.
    fn follow_fullscreen(&self) {
        let imp = self.imp();
        if self.is_fullscreen() {
            imp.collapsed_before_fullscreen
                .set(Some(imp.split_view.is_collapsed()));
            imp.split_view.set_collapsed(true);
            imp.split_view.set_show_content(true);
        } else if let Some(collapsed) = imp.collapsed_before_fullscreen.take() {
            imp.split_view.set_collapsed(collapsed);
        }
        imp.machine_view.set_fullscreen(self.is_fullscreen());
    }
}

fn connection_label(uri: &str) -> String {
    match uri {
        prefs::SYSTEM_URI => gettext("System"),
        prefs::SESSION_URI => gettext("User Session"),
        other => other.to_owned(),
    }
}
