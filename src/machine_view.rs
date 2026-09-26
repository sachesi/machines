//! `MachinesMachineView`: the selected machine, its console or its details, and the
//! `machine.*` actions that drive it.

use std::cell::{Cell, OnceCell, RefCell};
use std::collections::HashSet;
use std::os::fd::{FromRawFd, OwnedFd};
use std::rc::Rc;

use gettextrs::gettext;
use vte4::prelude::*;

use crate::adw::prelude::*;
use crate::adw::subclass::prelude::*;
use crate::console::{Console, FdSource};
use crate::domain_xml::{DiskDevice, MachineConfig};
use crate::hypervisor::{Change, Hypervisor, MachineInfo, MachineState, Result, SerialStream};
use crate::machine::Machine;
use crate::window::MachinesWindow;
use crate::{adw, details, dialogs, gio, glib, gtk, keymap, usage};

/// How close to the top edge the pointer has to come, in fullscreen, to bring up the
/// console's controls.
const REVEAL_EDGE: f64 = 4.0;
/// How close to the top edge a touch has to land, in fullscreen, for the same.
const TOUCH_REVEAL_EDGE: f64 = 24.0;
/// How long the console says how to give the keyboard back, once it takes it.
const GRAB_HINT_TIME: std::time::Duration = std::time::Duration::from_secs(3);
/// How long the controls stay when they come down by themselves or for a touch.
const CONTROLS_PEEK_TIME: std::time::Duration = std::time::Duration::from_secs(3);

mod imp {
    use super::*;

    #[derive(Default, gtk::CompositeTemplate)]
    #[template(resource = "/io/github/sachesi/machines/ui/machine_view.ui")]
    pub struct MachineView {
        #[template_child]
        pub toolbar: TemplateChild<adw::ToolbarView>,
        #[template_child]
        pub title: TemplateChild<adw::WindowTitle>,
        #[template_child]
        pub view_toggle: TemplateChild<adw::ToggleGroup>,
        #[template_child]
        pub view_stack: TemplateChild<adw::ViewStack>,
        #[template_child]
        pub serial_page: TemplateChild<adw::ViewStackPage>,
        #[template_child]
        pub start_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub power_button: TemplateChild<gtk::MenuButton>,
        #[template_child]
        pub fullscreen_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub console_place: TemplateChild<gtk::Stack>,
        #[template_child]
        pub console_bin: TemplateChild<adw::Bin>,
        #[template_child]
        pub console_overlay: TemplateChild<gtk::Overlay>,
        #[template_child]
        pub console_stack: TemplateChild<gtk::Stack>,
        #[template_child]
        pub grab_hint: TemplateChild<gtk::Revealer>,
        #[template_child]
        pub console_controls: TemplateChild<gtk::Revealer>,
        #[template_child]
        pub controls_title: TemplateChild<gtk::Label>,
        #[template_child]
        pub leave_fullscreen_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub console: TemplateChild<Console>,
        #[template_child]
        pub console_message: TemplateChild<adw::StatusPage>,
        #[template_child]
        pub console_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub serial_stack: TemplateChild<gtk::Stack>,
        #[template_child]
        pub serial_bin: TemplateChild<adw::Bin>,
        #[template_child]
        pub serial_hint: TemplateChild<gtk::Label>,
        #[template_child]
        pub serial_message: TemplateChild<adw::StatusPage>,
        #[template_child]
        pub serial_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub details_scroller: TemplateChild<gtk::ScrolledWindow>,
        #[template_child]
        pub details_start: TemplateChild<gtk::Box>,
        #[template_child]
        pub details_end: TemplateChild<gtk::Box>,

        pub(super) machine: RefCell<Option<Machine>>,
        pub(super) changed_handler: RefCell<Option<glib::SignalHandlerId>>,
        /// What the details page was last built from.
        pub(super) shown: RefCell<Option<MachineInfo>>,
        pub(super) host_files: RefCell<details::HostFiles>,
        /// Whether the machine was running when last seen, for the page to follow it as
        /// it starts and stops.
        pub(super) was_active: Cell<Option<bool>>,
        /// The machines on their way to running, by UUID.
        pub(super) starting: RefCell<HashSet<String>>,
        /// The rows of the details left expanded, by a key of their device, so they stay
        /// so as the details are filled again after each change.
        pub(super) expanded: RefCell<HashSet<String>>,
        /// Bumped whenever the machine changes, so a display socket that arrives for the
        /// previous one is closed rather than shown.
        pub(super) generation: Cell<u64>,
        pub(super) connecting: Cell<bool>,
        /// Why the display went away while the machine kept running.
        pub(super) console_error: RefCell<Option<String>>,
        pub(super) fullscreen: Cell<bool>,
        /// What the console's button does, by action name.
        pub(super) console_action: RefCell<String>,
        /// The window the display is in while it is out of this view, and its title.
        pub(super) detached: RefCell<Option<(adw::Window, adw::WindowTitle)>>,
        pub(super) usage: Rc<RefCell<usage::History>>,
        pub(super) terminal: OnceCell<vte4::Terminal>,
        pub(super) serial: RefCell<Option<SerialStream>>,
        pub(super) serial_connecting: Cell<bool>,
        /// Bumped whenever the serial console closes, so what arrives from the one before
        /// is dropped.
        pub(super) serial_generation: Cell<u64>,
        pub(super) serial_error: RefCell<Option<String>>,
        /// The view toggle's toggles in order, the serial console's among them while the
        /// group leaves it out.
        pub(super) view_toggles: OnceCell<Vec<adw::Toggle>>,
        pub(super) view_toggle_binding: RefCell<Option<glib::Binding>>,
        /// `console.*`: the actions of the controls that go with the display, wherever it
        /// is.
        pub(super) console_actions: gio::SimpleActionGroup,
        pub(super) grab_hint_timeout: RefCell<Option<glib::SourceId>>,
        /// While set, the controls stay down wherever the pointer goes.
        pub(super) controls_timeout: RefCell<Option<glib::SourceId>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for MachineView {
        const NAME: &'static str = "MachinesMachineView";
        type Type = super::MachineView;
        type ParentType = adw::BreakpointBin;

        fn class_init(klass: &mut Self::Class) {
            Console::ensure_type();
            klass.bind_template();
            install_actions(klass);
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for MachineView {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            let toggles = ["console", "serial", "details"]
                .into_iter()
                .filter_map(|name| self.view_toggle.toggle_by_name(name))
                .collect();
            let _ = self.view_toggles.set(toggles);
            obj.bind_view_toggle();
            self.console.connect_connected(glib::clone!(
                #[weak(rename_to = view)]
                obj,
                move |_| {
                    view.imp().console_stack.set_visible_child_name("display");
                    view.update_actions();
                }
            ));
            self.console.connect_disconnected(glib::clone!(
                #[weak(rename_to = view)]
                obj,
                move |_, reason| {
                    let reason = if reason.is_empty() {
                        gettext("The display connection closed.")
                    } else {
                        reason.to_owned()
                    };
                    view.imp().console_error.replace(Some(reason));
                    view.update();
                    // Most likely the machine stopped; the listing will say.
                    if let Some(win) = view.window() {
                        win.refresh();
                    }
                }
            ));

            let terminal = vte4::Terminal::builder()
                .hexpand(true)
                .vexpand(true)
                .scrollback_lines(10_000)
                .build();
            terminal.connect_commit(glib::clone!(
                #[weak(rename_to = view)]
                obj,
                move |_, text, _| {
                    if let Some(serial) = &*view.imp().serial.borrow() {
                        serial.send(text.as_bytes());
                    }
                }
            ));
            self.serial_bin.set_child(Some(&terminal));
            let _ = self.terminal.set(terminal);

            // Not action names on the buttons: they go with the display when that moves to
            // a window of its own, out of reach of the view's actions.
            self.console_button.connect_clicked(glib::clone!(
                #[weak(rename_to = view)]
                obj,
                move |_| {
                    let action = view.imp().console_action.borrow().clone();
                    let _ = view.activate_action(&action, None);
                }
            ));
            self.leave_fullscreen_button.connect_clicked(glib::clone!(
                #[weak(rename_to = view)]
                obj,
                move |_| {
                    let _ = view.activate_action("machine.fullscreen", None);
                }
            ));
            obj.setup_console_controls();

            // The display is connected only while it is on screen, so nothing holds it
            // while the details show or the window is gone.
            self.view_stack
                .connect_visible_child_name_notify(glib::clone!(
                    #[weak(rename_to = view)]
                    obj,
                    move |_| view.update()
                ));
            obj.connect_map(|view| view.update());
            obj.connect_unmap(|view| view.update());

            let settings = crate::prefs::settings();
            let details = gio::SimpleActionGroup::new();
            details.add_action(&settings.create_action("show-advanced-settings"));
            obj.insert_action_group("details", Some(&details));
            settings.connect_changed(
                Some("show-advanced-settings"),
                glib::clone!(
                    #[weak(rename_to = view)]
                    obj,
                    move |_, _| view.refresh_details()
                ),
            );
            obj.update();
        }
    }

    impl WidgetImpl for MachineView {}
    impl BreakpointBinImpl for MachineView {}
}

glib::wrapper! {
    pub struct MachineView(ObjectSubclass<imp::MachineView>)
        @extends adw::BreakpointBin, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

fn install_actions(klass: &mut <imp::MachineView as ObjectSubclass>::Class) {
    klass.install_action("machine.start", None, |view, _, _| view.start());
    klass.install_action("machine.shut-down", None, |view, _, _| {
        view.run(|hv, uuid| hv.shut_down(uuid));
    });
    klass.install_action("machine.save", None, |view, _, _| {
        view.run(|hv, uuid| hv.save(uuid));
    });
    klass.install_action_async("machine.discard-saved", None, |view, _, _| async move {
        let confirmed = dialogs::confirm(
            &view,
            &gettext("Discard Saved State?"),
            &gettext(
                "The virtual machine starts afresh next time instead of resuming where it \
                 was saved. Work not saved in it is lost.",
            ),
            &gettext("_Discard"),
        )
        .await;
        if confirmed {
            view.run(|hv, uuid| hv.discard_saved(uuid));
        }
    });
    klass.install_action("machine.pause", None, |view, _, _| {
        view.run(|hv, uuid| hv.pause(uuid));
    });
    klass.install_action("machine.reboot", None, |view, _, _| {
        view.run(|hv, uuid| hv.reboot(uuid));
    });
    klass.install_action("machine.resume", None, |view, _, _| {
        view.run(|hv, uuid| hv.start(uuid));
    });
    klass.install_action_async("machine.reset", None, |view, _, _| async move {
        let confirmed = dialogs::confirm(
            &view,
            &gettext("Force Reset?"),
            &gettext(
                "The virtual machine restarts at once, as if its reset button were pressed. \
                 Work not saved in it is lost.",
            ),
            &gettext("Force _Reset"),
        )
        .await;
        if confirmed {
            view.run(|hv, uuid| hv.reset(uuid));
        }
    });
    klass.install_action_async("machine.force-off", None, |view, _, _| async move {
        let confirmed = dialogs::confirm(
            &view,
            &gettext("Force Off?"),
            &gettext(
                "The virtual machine stops at once, as if its power were cut. Work not saved \
                 in it is lost, and its disks may be left inconsistent.",
            ),
            &gettext("Force _Off"),
        )
        .await;
        if confirmed {
            view.run(|hv, uuid| hv.force_off(uuid));
        }
    });
    klass.install_action("machine.reconnect", None, |view, _, _| {
        view.imp().console_error.take();
        view.update();
    });
    klass.install_action("machine.fullscreen", None, |view, _, _| {
        let window = match &*view.imp().detached.borrow() {
            Some((window, _)) => Some(window.clone().upcast::<gtk::Window>()),
            None => view.window().map(Cast::upcast),
        };
        if let Some(window) = window {
            window.set_fullscreened(!window.is_fullscreen());
        }
    });
    klass.install_action("machine.detach-console", None, |view, _, _| {
        view.detach_console();
    });
    klass.install_action("machine.attach-console", None, |view, _, _| {
        view.attach_console();
    });
    klass.install_action("machine.reconnect-serial", None, |view, _, _| {
        view.imp().serial_error.take();
        view.update();
    });
    klass.install_action("machine.delete", None, |view, _, _| view.delete());
    klass.install_action("machine.rename", None, |view, _, _| {
        dialogs::machine::rename(view);
    });
    klass.install_action("machine.screenshot", None, |view, _, _| {
        dialogs::machine::screenshot(view);
    });
    klass.install_action("machine.edit-xml", None, |view, _, _| {
        dialogs::machine::edit_xml(view);
    });
    klass.install_action("machine.clone", None, |view, _, _| {
        dialogs::machine::clone(view);
    });
    klass.install_action("machine.usb-devices", None, |view, _, _| {
        dialogs::hardware::plug_usb(view);
    });
    klass.install_action("machine.redirect-usb", None, |view, _, _| {
        dialogs::hardware::redirect_usb(view);
    });
}

/// What the page of a machine that is not running says under its state, and the label of
/// its button that starts it.
fn stopped(info: &MachineInfo) -> (Option<String>, String) {
    if info.saved {
        (
            Some(gettext("It resumes where it was saved.")),
            gettext("_Resume"),
        )
    } else {
        (None, gettext("_Start"))
    }
}

/// Close the serial console where closing it, which waits on libvirt's answer, holds up
/// nothing on screen.
fn close_stream(serial: SerialStream) {
    gio::spawn_blocking(move || drop(serial));
}

/// Whether the running machine has a screen for the console to show: a display it can
/// connect to, and a video card to draw it, which Looking Glass and a passed-through
/// graphics card do without.
fn has_screen(info: &MachineInfo) -> bool {
    info.live
        .as_ref()
        .or(info.config.as_ref())
        .is_some_and(|c| {
            c.graphics.iter().any(|g| g == "vnc" || g == "spice")
                && c.video.as_deref() != Some("none")
        })
}

impl MachineView {
    fn window(&self) -> Option<MachinesWindow> {
        self.root().and_downcast()
    }

    fn machine(&self) -> Option<Machine> {
        self.imp().machine.borrow().clone()
    }

    pub fn info(&self) -> Option<MachineInfo> {
        self.machine().and_then(|m| m.info())
    }

    pub fn usage_history(&self) -> Rc<RefCell<usage::History>> {
        self.imp().usage.clone()
    }

    /// Run `f` on the selected machine's UUID, off the main loop.
    pub fn run<F>(&self, f: F)
    where
        F: FnOnce(&Hypervisor, &str) -> Result<()> + Send + 'static,
    {
        if let (Some(win), Some(machine)) = (self.window(), self.machine()) {
            let uuid = machine.uuid();
            win.run(move |hv| f(hv, &uuid));
        }
    }

    /// Like [`Self::run`], for a change of hardware, which the running machine may only
    /// get at its next start.
    pub fn change<F>(&self, f: F)
    where
        F: FnOnce(&Hypervisor, &str) -> Result<Change> + Send + 'static,
    {
        let (Some(win), Some(machine)) = (self.window(), self.machine()) else {
            return;
        };
        let uuid = machine.uuid();
        glib::spawn_future_local(async move {
            match win.call(move |hv| f(hv, &uuid)).await {
                Some(Ok(Change::AtNextStart)) => win.toast(&gettext(
                    "The change takes effect the next time the virtual machine starts",
                )),
                Some(Err(e)) => win.toast(&e),
                _ => {}
            }
            win.refresh();
        });
    }

    /// Start the machine, which can take a while, as QEMU sets up its memory and takes
    /// its devices; its start button spins until then.
    fn start(&self) {
        let (Some(win), Some(machine)) = (self.window(), self.machine()) else {
            return;
        };
        let uuid = machine.uuid();
        if !self.imp().starting.borrow_mut().insert(uuid.clone()) {
            return;
        }
        self.update();
        glib::spawn_future_local(glib::clone!(
            #[weak(rename_to = view)]
            self,
            async move {
                let id = uuid.clone();
                let started = win
                    .call(move |hv| {
                        hv.start(&id)?;
                        Ok(hv.machine(&id).ok())
                    })
                    .await;
                view.imp().starting.borrow_mut().remove(&uuid);
                match started {
                    // Running now, not only once the machines are next listed.
                    Some(Ok(Some(info))) => machine.update(info),
                    Some(Err(e)) => win.toast(&e),
                    _ => {}
                }
                view.update();
                win.refresh();
            }
        ));
    }

    pub fn set_machine(&self, machine: Option<&Machine>) {
        let imp = self.imp();
        if imp.machine.borrow().as_ref() == machine {
            return;
        }
        // While the old machine is still this view's, for the details page to save what it
        // has waiting as it goes.
        self.clear_details();
        if let (Some(old), Some(handler)) = (imp.machine.take(), imp.changed_handler.take()) {
            old.disconnect(handler);
        }
        self.put_back_console();
        self.close_serial();
        imp.serial_error.take();
        if let Some(terminal) = imp.terminal.get() {
            terminal.reset(true, true);
        }
        imp.serial_hint.set_visible(true);
        imp.generation.set(imp.generation.get() + 1);
        imp.connecting.set(false);
        imp.console_error.take();
        imp.console.close();
        imp.shown.take();
        imp.was_active.take();
        imp.expanded.take();
        imp.details_scroller.vadjustment().set_value(0.0);
        if let Some(win) = self.window().filter(|w| w.is_fullscreen()) {
            win.unfullscreen();
        }
        if let Some(machine) = machine {
            let handler = machine.connect_changed(glib::clone!(
                #[weak(rename_to = view)]
                self,
                move |_| view.update()
            ));
            imp.changed_handler.replace(Some(handler));
        }
        imp.machine.replace(machine.cloned());
        self.update();
    }

    fn bind_view_toggle(&self) {
        let imp = self.imp();
        let binding = imp
            .view_stack
            .bind_property("visible-child-name", &*imp.view_toggle, "active-name")
            .bidirectional()
            .sync_create()
            .build();
        imp.view_toggle_binding.replace(Some(binding));
    }

    /// Offer the serial console only to a machine with a serial port, or one it gets at
    /// its next start.
    fn offer_serial(&self, offered: bool) {
        let imp = self.imp();
        if !offered && imp.view_stack.visible_child_name().as_deref() == Some("serial") {
            // Updates the view again, which comes back here with the console showing.
            self.show_console();
        }
        if imp.view_toggle.toggle_by_name("serial").is_some() == offered {
            return;
        }
        imp.serial_page.set_visible(offered);
        // A group only adds at its end, and the serial console goes between the other two.
        // Unbound meanwhile, for the page not to follow the group through the gap.
        if let Some(binding) = imp.view_toggle_binding.take() {
            binding.unbind();
        }
        let toggles = imp.view_toggles.get().into_iter().flatten();
        // Not remove_all(), which leaves the toggles marked as the group's, so that it
        // refuses them back.
        for toggle in toggles.clone() {
            if imp.view_toggle.toggle_by_name(&toggle.name()).is_some() {
                imp.view_toggle.remove(toggle);
            }
        }
        for toggle in toggles {
            if offered || toggle.name() != "serial" {
                imp.view_toggle.add(toggle.clone());
            }
        }
        self.bind_view_toggle();
    }

    pub fn show_console(&self) {
        self.imp().view_stack.set_visible_child_name("console");
    }

    pub fn set_fullscreen(&self, fullscreen: bool) {
        let imp = self.imp();
        imp.fullscreen.set(fullscreen);
        imp.toolbar.set_reveal_top_bars(!fullscreen);
        imp.toolbar.set_extend_content_to_top_edge(fullscreen);
        self.follow_console_fullscreen(fullscreen);
        if fullscreen {
            self.show_console();
            imp.console.grab_focus();
        }
    }

    /// Show the controls for a moment as the display goes fullscreen, for them to be
    /// found where they come down, and take them away as it leaves.
    fn follow_console_fullscreen(&self, fullscreen: bool) {
        if fullscreen {
            self.peek_console_controls();
        } else {
            if let Some(timeout) = self.imp().controls_timeout.take() {
                timeout.remove();
            }
            self.imp().console_controls.set_reveal_child(false);
        }
    }

    /// The controls over the display: the keys to send, and in fullscreen the bar that
    /// comes down while the pointer is at the top edge.
    fn setup_console_controls(&self) {
        let imp = self.imp();
        let send_keys = gio::SimpleAction::new("send-keys", Some(glib::VariantTy::STRING));
        send_keys.connect_activate(glib::clone!(
            #[weak(rename_to = view)]
            self,
            move |_, target| {
                use keymap::*;
                let keys = match target.and_then(|t| t.str()) {
                    Some("ctrl-alt-delete") => [KEY_LEFTCTRL, KEY_LEFTALT, KEY_DELETE],
                    Some("ctrl-alt-backspace") => [KEY_LEFTCTRL, KEY_LEFTALT, KEY_BACKSPACE],
                    Some("ctrl-alt-f1") => [KEY_LEFTCTRL, KEY_LEFTALT, KEY_F1],
                    Some("ctrl-alt-f2") => [KEY_LEFTCTRL, KEY_LEFTALT, KEY_F2],
                    Some("ctrl-alt-f7") => [KEY_LEFTCTRL, KEY_LEFTALT, KEY_F7],
                    _ => return,
                };
                view.run(move |hv, uuid| hv.send_keys(uuid, &keys));
            }
        ));
        imp.console_actions.add_action(&send_keys);
        // The view's own for its menu, and the display's for wherever it goes.
        self.insert_action_group("console", Some(&imp.console_actions));
        imp.console_overlay
            .insert_action_group("console", Some(&imp.console_actions));

        let motion = gtk::EventControllerMotion::new();
        motion.set_propagation_phase(gtk::PropagationPhase::Capture);
        motion.connect_motion(glib::clone!(
            #[weak(rename_to = view)]
            self,
            move |_, _, y| view.reveal_console_controls(y)
        ));
        imp.console_overlay.add_controller(motion);

        // A touch screen has no pointer to come to the edge, so a tap there does.
        let touch = gtk::GestureClick::new();
        touch.set_touch_only(true);
        touch.set_propagation_phase(gtk::PropagationPhase::Capture);
        touch.connect_pressed(glib::clone!(
            #[weak(rename_to = view)]
            self,
            move |_, _, _, y| {
                if y <= TOUCH_REVEAL_EDGE && view.console_is_fullscreen() {
                    view.peek_console_controls();
                }
            }
        ));
        imp.console_overlay.add_controller(touch);

        imp.console.connect_grab_changed(glib::clone!(
            #[weak(rename_to = view)]
            self,
            move |_, grabbed| view.show_grab_hint(grabbed)
        ));
    }

    /// Bring the controls down while the pointer, at `y` over the display, is at the top
    /// edge of a fullscreen window, and keep them until it goes below them.
    fn reveal_console_controls(&self, y: f64) {
        let imp = self.imp();
        let controls = &imp.console_controls;
        if !self.console_is_fullscreen() {
            controls.set_reveal_child(false);
        } else if y <= REVEAL_EDGE {
            controls.set_reveal_child(true);
            imp.grab_hint.set_reveal_child(false);
        } else if y > f64::from(controls.height()) + REVEAL_EDGE
            && imp.controls_timeout.borrow().is_none()
        {
            controls.set_reveal_child(false);
        }
    }

    /// Bring the controls down for [`CONTROLS_PEEK_TIME`], and longer while the pointer
    /// is on them.
    fn peek_console_controls(&self) {
        let imp = self.imp();
        if let Some(timeout) = imp.controls_timeout.take() {
            timeout.remove();
        }
        imp.console_controls.set_reveal_child(true);
        imp.grab_hint.set_reveal_child(false);
        let timeout = glib::timeout_add_local_once(
            CONTROLS_PEEK_TIME,
            glib::clone!(
                #[weak(rename_to = view)]
                self,
                move || {
                    let controls = &view.imp().console_controls;
                    view.imp().controls_timeout.take();
                    if !controls.state_flags().contains(gtk::StateFlags::PRELIGHT) {
                        controls.set_reveal_child(false);
                    }
                }
            ),
        );
        imp.controls_timeout.replace(Some(timeout));
    }

    fn console_is_fullscreen(&self) -> bool {
        self.imp()
            .console_overlay
            .root()
            .and_downcast::<gtk::Window>()
            .is_some_and(|w| w.is_fullscreen())
    }

    fn show_grab_hint(&self, shown: bool) {
        let imp = self.imp();
        if let Some(timeout) = imp.grab_hint_timeout.take() {
            timeout.remove();
        }
        imp.grab_hint
            .set_reveal_child(shown && !imp.console_controls.reveals_child());
        if shown {
            let timeout = glib::timeout_add_local_once(
                GRAB_HINT_TIME,
                glib::clone!(
                    #[weak(rename_to = view)]
                    self,
                    move || {
                        view.imp().grab_hint_timeout.take();
                        view.imp().grab_hint.set_reveal_child(false);
                    }
                ),
            );
            imp.grab_hint_timeout.replace(Some(timeout));
        }
    }

    fn update(&self) {
        let imp = self.imp();
        let Some(info) = self.info() else {
            self.update_actions();
            return;
        };
        imp.title.set_title(&info.name);
        imp.title.set_subtitle(&info.status());
        imp.controls_title.set_label(&info.name);
        imp.console
            .update_property(&[gtk::accessible::Property::Label(&info.name)]);
        if let Some((window, title)) = &*imp.detached.borrow() {
            window.set_title(Some(&info.name));
            title.set_title(&info.name);
            title.set_subtitle(&info.status());
        }
        let resumable =
            matches!(info.state, MachineState::Paused | MachineState::Suspended) || info.saved;
        let starting = imp.starting.borrow().contains(&info.uuid);
        imp.start_button
            .set_visible(!info.state.is_active() || resumable);
        if starting {
            imp.title.set_subtitle(&gettext("Starting…"));
            imp.start_button.set_child(Some(&adw::Spinner::new()));
            imp.start_button
                .set_tooltip_text(Some(&gettext("Starting…")));
        } else {
            imp.start_button
                .set_icon_name("media-playback-start-symbolic");
            imp.start_button.set_tooltip_text(Some(&if resumable {
                gettext("Resume")
            } else {
                gettext("Start")
            }));
        }
        imp.power_button.set_visible(info.state.is_active());
        if !info.state.is_active() {
            imp.console_error.take();
        }
        self.follow_state(&info);
        self.update_actions();
        self.update_console(&info);
        self.update_serial(&info);
        if imp.shown.borrow().as_ref() != Some(&info) {
            // Focus left in a group that goes would move to the first row of the new
            // page, which the page would scroll to; without it, the page stays put.
            if let Some(root) = self.root()
                && root
                    .focus()
                    .is_some_and(|f| f.is_ancestor(&*imp.details_scroller))
            {
                root.set_focus(gtk::Widget::NONE);
            }
            self.clear_details();
            let files = imp.host_files.borrow().clone();
            details::fill(self, &info, &files, &imp.details_start, &imp.details_end);
            imp.shown.replace(Some(info));
            self.read_host_files();
        }
    }

    /// Read what the details show of the host's own files again, off the main loop, and
    /// fill the details again if it changed.
    fn read_host_files(&self) {
        let (Some(win), Some(info)) = (self.window(), self.info()) else {
            return;
        };
        let host = win.host();
        glib::spawn_future_local(glib::clone!(
            #[weak(rename_to = view)]
            self,
            async move {
                let read = gio::spawn_blocking(move || details::HostFiles::read(info.uuid, host));
                let Ok(files) = read.await else {
                    return;
                };
                // Unless the view went on to another machine meanwhile.
                let Some(uuid) = view
                    .machine()
                    .map(|m| m.uuid())
                    .filter(|u| *u == files.uuid)
                else {
                    return;
                };
                let changed = view.imp().host_files.borrow().differ_for(&uuid, &files);
                view.imp().host_files.replace(files);
                if changed {
                    view.refresh_details();
                }
            }
        ));
    }

    /// Show the details of a machine that is not running, and its screen once it starts,
    /// where it has one to show.
    fn follow_state(&self, info: &MachineInfo) {
        let imp = self.imp();
        let active = info.state.is_active();
        let was_active = imp.was_active.replace(Some(active));
        if was_active == Some(active) || imp.fullscreen.get() {
            return;
        }
        let page = imp.view_stack.visible_child_name();
        let details = page.as_deref() == Some("details");
        let wanted = if active && has_screen(info) {
            // Only from the details, not to take the serial console away from its boot.
            (was_active.is_none() || details).then_some("console")
        } else if active {
            was_active.is_none().then_some("details")
        } else {
            Some("details")
        };
        if let Some(wanted) = wanted.filter(|w| page.as_deref() != Some(w)) {
            imp.view_stack.set_visible_child_name(wanted);
        }
    }

    pub fn is_expanded(&self, key: &str) -> bool {
        self.imp().expanded.borrow().contains(key)
    }

    pub fn set_expanded(&self, key: &str, expanded: bool) {
        let mut keys = self.imp().expanded.borrow_mut();
        if expanded {
            keys.insert(key.to_owned());
        } else {
            keys.remove(key);
        }
    }

    /// Fill the details again, for what they show beside the machine's own information.
    pub fn refresh_details(&self) {
        self.imp().shown.take();
        self.update();
    }

    fn clear_details(&self) {
        let imp = self.imp();
        for column in [&*imp.details_start, &*imp.details_end] {
            while let Some(child) = column.first_child() {
                column.remove(&child);
            }
        }
    }

    fn update_actions(&self) {
        let info = self.info();
        let state = info.as_ref().map(|i| i.state);
        let running = state == Some(MachineState::Running);
        let active = state.is_some_and(MachineState::is_active);
        let starting = info
            .as_ref()
            .is_some_and(|i| self.imp().starting.borrow().contains(&i.uuid));
        self.action_set_enabled(
            "machine.start",
            state.is_some() && !running && state != Some(MachineState::ShuttingDown) && !starting,
        );
        self.action_set_enabled("machine.shut-down", running);
        self.action_set_enabled("machine.pause", running);
        let persistent = info.as_ref().is_some_and(|i| i.persistent);
        self.action_set_enabled(
            "machine.save",
            persistent && matches!(state, Some(MachineState::Running | MachineState::Paused)),
        );
        let saved = info.as_ref().is_some_and(|i| i.saved);
        self.action_set_enabled("machine.discard-saved", saved && !active);
        self.action_set_enabled(
            "machine.resume",
            matches!(state, Some(MachineState::Paused | MachineState::Suspended)),
        );
        self.action_set_enabled("machine.reboot", running);
        self.action_set_enabled("machine.reset", active);
        self.action_set_enabled("machine.force-off", active);
        if let Some(keys) = self
            .imp()
            .console_actions
            .lookup_action("send-keys")
            .and_downcast::<gio::SimpleAction>()
        {
            keys.set_enabled(running);
        }
        self.action_set_enabled("machine.screenshot", running);
        self.action_set_enabled("machine.usb-devices", running);
        self.action_set_enabled(
            "machine.redirect-usb",
            running && self.usb_redirection().is_some(),
        );
        self.action_set_enabled("machine.delete", state.is_some());
        let editable = persistent && !active;
        // libvirt keeps a saved state under the machine's name.
        self.action_set_enabled("machine.rename", editable && !saved);
        self.action_set_enabled("machine.clone", editable);
        self.action_set_enabled("machine.edit-xml", persistent);
        let imp = self.imp();
        let detached = imp.detached.borrow().is_some();
        // The console is all fullscreen and a window of its own are for.
        let screen = active && info.as_ref().is_some_and(has_screen);
        self.action_set_enabled(
            "machine.fullscreen",
            screen || detached || imp.fullscreen.get(),
        );
        imp.fullscreen_button.set_visible(screen && !detached);
        self.action_set_enabled(
            "machine.detach-console",
            screen && !detached && !imp.fullscreen.get(),
        );
        self.action_set_enabled("machine.attach-console", detached);
    }

    fn console_message(
        &self,
        icon: &str,
        title: &str,
        text: Option<&str>,
        button: Option<(&str, &str)>,
    ) {
        let imp = self.imp();
        imp.console_message.set_icon_name(Some(icon));
        imp.console_message.set_title(title);
        imp.console_message
            .set_description(text.map(glib::markup_escape_text).as_deref());
        imp.console_button.set_visible(button.is_some());
        if let Some((label, action)) = button {
            imp.console_button.set_label(label);
            imp.console_action.replace(action.to_owned());
        }
        imp.console_stack.set_visible_child_name("message");
    }

    fn update_console(&self, info: &MachineInfo) {
        let imp = self.imp();
        if !info.state.is_active() {
            imp.console.close();
            let (text, start) = stopped(info);
            self.console_message(
                "system-shutdown-symbolic",
                &info.status(),
                text.as_deref(),
                Some((&start, "machine.start")),
            );
            return;
        }
        if !self.console_wanted() {
            self.release_console();
            return;
        }
        if imp.console.is_open() || imp.connecting.get() {
            return;
        }
        // With no video card, the display has nothing to show but what Looking Glass does.
        if let Some(live) = &info.live
            && live.looking_glass.is_some()
            && live.video.as_deref() == Some("none")
        {
            self.console_message(
                "video-display-symbolic",
                &gettext("Shown in Looking Glass"),
                Some(&gettext(
                    "The guest shows its screen on the passed-through graphics card. Open it \
                     with the Looking Glass client.",
                )),
                None,
            );
            return;
        }
        let live_graphics = info.live.as_ref().and_then(|l| l.graphics.first());
        match live_graphics.map(String::as_str) {
            Some(protocol @ ("vnc" | "spice")) => {
                if let Some(error) = imp.console_error.borrow().as_deref() {
                    self.console_message(
                        "video-display-symbolic",
                        &gettext("Display Disconnected"),
                        Some(error),
                        Some((&gettext("_Reconnect"), "machine.reconnect")),
                    );
                } else {
                    self.open_console(protocol == "spice");
                }
            }
            Some(other) => self.console_message(
                "video-display-symbolic",
                &gettext("Unsupported Display"),
                Some(
                    &gettext("The console cannot show a “{type}” display.")
                        .replace("{type}", other),
                ),
                None,
            ),
            None => self.console_message(
                "video-display-symbolic",
                &gettext("No Display"),
                Some(&gettext("This virtual machine has no graphical display.")),
                None,
            ),
        }
    }

    /// While the guest has USB devices through the console, the console stays, not to pull
    /// them out from under it.
    fn console_wanted(&self) -> bool {
        let imp = self.imp();
        self.is_mapped() && imp.view_stack.visible_child_name().as_deref() == Some("console")
            || imp.detached.borrow().is_some()
            || imp.console.redirects_usb()
    }

    /// Move the display to a window of its own, to put it on another screen.
    fn detach_console(&self) {
        let imp = self.imp();
        let Some(info) = self.info() else {
            return;
        };
        if imp.detached.borrow().is_some() {
            return;
        }
        let title = adw::WindowTitle::new(&info.name, &info.status());
        let fullscreen = gtk::Button::builder()
            .icon_name("view-fullscreen-symbolic")
            .tooltip_text(gettext("Fullscreen"))
            .build();
        let header = adw::HeaderBar::builder().title_widget(&title).build();
        header.pack_end(&fullscreen);
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        let (width, height) = (imp.console.width(), imp.console.height());
        imp.console_bin.set_child(gtk::Widget::NONE);
        toolbar.set_content(Some(&*imp.console_overlay));
        let window = adw::Window::builder()
            .title(&info.name)
            .content(&toolbar)
            .default_width(width.max(640))
            .default_height(height.max(480))
            .build();
        window.set_application(self.window().and_then(|w| w.application()).as_ref());
        fullscreen.connect_clicked(glib::clone!(
            #[weak]
            window,
            move |_| window.set_fullscreened(!window.is_fullscreen())
        ));
        window.connect_fullscreened_notify(glib::clone!(
            #[weak]
            toolbar,
            #[weak]
            fullscreen,
            #[weak(rename_to = view)]
            self,
            move |window| {
                let on = window.is_fullscreen();
                toolbar.set_reveal_top_bars(!on);
                toolbar.set_extend_content_to_top_edge(on);
                view.follow_console_fullscreen(on);
                fullscreen.set_icon_name(if on {
                    "view-restore-symbolic"
                } else {
                    "view-fullscreen-symbolic"
                });
                fullscreen.set_tooltip_text(Some(&if on {
                    gettext("Leave Fullscreen")
                } else {
                    gettext("Fullscreen")
                }));
            }
        ));
        window.connect_close_request(glib::clone!(
            #[weak(rename_to = view)]
            self,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |_| {
                view.attach_console();
                glib::Propagation::Stop
            }
        ));
        imp.console_place.set_visible_child_name("away");
        imp.detached.replace(Some((window.clone(), title)));
        window.present();
        imp.console.grab_focus();
        self.update();
    }

    /// Put the display back into this view from its own window, if it is in one.
    pub fn attach_console(&self) {
        if self.put_back_console() {
            self.update();
        }
    }

    /// Whether the display was in a window of its own, which it now leaves.
    fn put_back_console(&self) -> bool {
        let imp = self.imp();
        let Some((window, _)) = imp.detached.take() else {
            return false;
        };
        if let Some(toolbar) = window.content().and_downcast::<adw::ToolbarView>() {
            toolbar.set_content(gtk::Widget::NONE);
        }
        imp.console_bin.set_child(Some(&*imp.console_overlay));
        imp.console_place.set_visible_child_name("here");
        window.destroy();
        true
    }

    pub fn usb_redirection(&self) -> Option<spice_client_glib::UsbDeviceManager> {
        self.imp().console.usb_redirection()
    }

    /// Keep the display or let it go, as the console and its USB devices now need.
    pub fn recheck_console(&self) {
        self.update();
    }

    /// Open the display afresh, for what the machine gained since it was opened.
    pub fn reconnect_console(&self) {
        self.release_console();
        self.update();
    }

    /// Let go of the display, or of the socket still on its way to it.
    fn release_console(&self) {
        let imp = self.imp();
        if imp.connecting.replace(false) {
            imp.generation.set(imp.generation.get() + 1);
        }
        imp.console.close();
    }

    fn open_console(&self, spice: bool) {
        let imp = self.imp();
        let (Some(win), Some(machine)) = (self.window(), self.machine()) else {
            return;
        };
        imp.connecting.set(true);
        imp.console_stack.set_visible_child_name("connecting");
        let generation = imp.generation.get();
        let uuid = machine.uuid();
        glib::spawn_future_local(glib::clone!(
            #[weak(rename_to = view)]
            self,
            async move {
                let opened = win.call(move |hv| hv.open_display(&uuid)).await;
                let imp = view.imp();
                if imp.generation.get() != generation {
                    if let Some(Ok(fd)) = opened {
                        // SAFETY: libvirt handed this descriptor over and nothing else has it.
                        drop(unsafe { OwnedFd::from_raw_fd(fd) });
                    }
                    return;
                }
                imp.connecting.set(false);
                match opened {
                    Some(Ok(fd)) if spice => imp.console.open_spice(fd, view.fd_source()),
                    Some(Ok(fd)) => imp.console.open(fd),
                    Some(Err(e)) => {
                        imp.console_error.replace(Some(e));
                        view.update();
                    }
                    None => {}
                }
            }
        ));
    }

    fn serial_message(
        &self,
        icon: &str,
        title: &str,
        text: Option<&str>,
        button: Option<(&str, &str)>,
    ) {
        let imp = self.imp();
        imp.serial_message.set_icon_name(Some(icon));
        imp.serial_message.set_title(title);
        imp.serial_message
            .set_description(text.map(glib::markup_escape_text).as_deref());
        imp.serial_button.set_visible(button.is_some());
        if let Some((label, action)) = button {
            imp.serial_button.set_label(label);
            imp.serial_button.set_action_name(Some(action));
        }
        imp.serial_stack.set_visible_child_name("message");
    }

    /// Open the serial console while its page shows, and close it otherwise.
    fn update_serial(&self, info: &MachineInfo) {
        let imp = self.imp();
        let has = |c: Option<&MachineConfig>| c.is_some_and(|c| c.serial);
        self.offer_serial(has(info.live.as_ref()) || has(info.config.as_ref()));
        if !info.state.is_active() {
            self.close_serial();
            imp.serial_error.take();
            let (text, start) = stopped(info);
            self.serial_message(
                "system-shutdown-symbolic",
                &info.status(),
                text.as_deref(),
                Some((&start, "machine.start")),
            );
            return;
        }
        if !has(info.live.as_ref().or(info.config.as_ref())) {
            // Offered only for the port the definition has for the next start.
            self.close_serial();
            self.serial_message(
                "utilities-terminal-symbolic",
                &gettext("No Serial Console"),
                Some(&gettext("The serial port comes with the next start.")),
                None,
            );
            return;
        }
        let wanted =
            self.is_mapped() && imp.view_stack.visible_child_name().as_deref() == Some("serial");
        if !wanted {
            self.close_serial();
            return;
        }
        if imp.serial.borrow().is_some() || imp.serial_connecting.get() {
            return;
        }
        if let Some(error) = imp.serial_error.borrow().as_deref() {
            self.serial_message(
                "utilities-terminal-symbolic",
                &gettext("Serial Console Closed"),
                Some(error),
                Some((&gettext("_Reconnect"), "machine.reconnect-serial")),
            );
            return;
        }
        self.open_serial();
    }

    fn open_serial(&self) {
        let imp = self.imp();
        let (Some(win), Some(machine)) = (self.window(), self.machine()) else {
            return;
        };
        imp.serial_connecting.set(true);
        let generation = imp.serial_generation.get();
        let weak = glib::SendWeakRef::from(self.downgrade());
        let sink = move |bytes: Option<Vec<u8>>| {
            let weak = weak.clone();
            glib::MainContext::default().invoke(move || {
                if let Some(view) = weak.upgrade() {
                    view.serial_received(generation, bytes);
                }
            });
        };
        let uuid = machine.uuid();
        glib::spawn_future_local(glib::clone!(
            #[weak(rename_to = view)]
            self,
            async move {
                let opened = win.call(move |hv| hv.open_serial(&uuid, sink)).await;
                let imp = view.imp();
                if imp.serial_generation.get() != generation {
                    if let Some(Ok(serial)) = opened {
                        close_stream(serial);
                    }
                    return;
                }
                imp.serial_connecting.set(false);
                match opened {
                    Some(Ok(serial)) => {
                        imp.serial.replace(Some(serial));
                        imp.serial_stack.set_visible_child_name("terminal");
                        if let Some(terminal) = imp.terminal.get() {
                            terminal.grab_focus();
                        }
                    }
                    Some(Err(e)) => {
                        imp.serial_error.replace(Some(e));
                        view.update();
                    }
                    None => {}
                }
            }
        ));
    }

    fn serial_received(&self, generation: u64, bytes: Option<Vec<u8>>) {
        let imp = self.imp();
        if imp.serial_generation.get() != generation {
            return;
        }
        match bytes {
            Some(bytes) => {
                imp.serial_hint.set_visible(false);
                if let Some(terminal) = imp.terminal.get() {
                    terminal.feed(&bytes);
                }
            }
            None => {
                self.close_serial();
                imp.serial_error
                    .replace(Some(gettext("The serial console closed.")));
                self.update();
            }
        }
    }

    /// Let go of the serial console, or of the one on its way.
    fn close_serial(&self) {
        let imp = self.imp();
        let connecting = imp.serial_connecting.replace(false);
        let serial = imp.serial.take();
        if serial.is_some() || connecting {
            imp.serial_generation.set(imp.serial_generation.get() + 1);
        }
        if let Some(serial) = serial {
            close_stream(serial);
        }
    }

    /// Sockets to the display for the channels of a SPICE session past its first.
    fn fd_source(&self) -> FdSource {
        let view = self.downgrade();
        Rc::new(move |done| {
            let (Some(win), Some(machine)) = (
                view.upgrade().and_then(|v| v.window()),
                view.upgrade().and_then(|v| v.machine()),
            ) else {
                done(None);
                return;
            };
            let uuid = machine.uuid();
            glib::spawn_future_local(async move {
                done(
                    win.call(move |hv| hv.open_display(&uuid))
                        .await
                        .and_then(Result::ok),
                );
            });
        })
    }

    fn delete(&self) {
        let (Some(win), Some(info)) = (self.window(), self.info()) else {
            return;
        };
        let others: Vec<String> = win
            .machine_infos()
            .into_iter()
            .filter(|m| m.uuid != info.uuid)
            .filter_map(|m| m.config)
            .flat_map(|c| c.disks.into_iter().filter_map(|d| d.source))
            .collect();
        let disks = info
            .config
            .as_ref()
            .map(|c| c.disks.as_slice())
            .unwrap_or_default();
        let images: Vec<String> = info
            .config
            .as_ref()
            .map(|c| c.disk_files())
            .unwrap_or_default()
            .into_iter()
            .filter(|f| !others.contains(f))
            .collect();
        let kept = disks
            .iter()
            .filter(|d| d.device == DiskDevice::Disk && d.source.is_some())
            .count()
            > images.len();
        glib::spawn_future_local(glib::clone!(
            #[weak(rename_to = view)]
            self,
            async move {
                let paths = images.clone();
                let Some(Ok(pooled)) = win.call(move |hv| Ok(hv.in_pools(&paths))).await else {
                    return;
                };
                // An image in no storage pool is most likely one the user brought in, not
                // one made for the machine: it is only deleted when ticked.
                let images: Vec<(String, bool)> = images.into_iter().zip(pooled).collect();
                let running = info.state.is_active();
                let Some(images) =
                    dialogs::delete::confirm(&view, &info.name, running, &images, kept).await
                else {
                    return;
                };
                let uuid = info.uuid.clone();
                match win.call(move |hv| hv.delete(&uuid, &images)).await {
                    Some(Ok(kept)) if !kept.is_empty() => win.toast(
                        &gettext("Some disk images could not be deleted: {files}")
                            .replace("{files}", &kept.join(", ")),
                    ),
                    Some(Err(e)) => win.toast(&e),
                    _ => {}
                }
                win.refresh();
            }
        ));
    }
}
