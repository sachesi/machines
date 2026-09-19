//! `MachinesMachineView`: the selected machine, its console or its details, and the
//! `machine.*` actions that drive it.

use std::cell::{Cell, RefCell};
use std::os::fd::{FromRawFd, OwnedFd};
use std::rc::Rc;

use gettextrs::gettext;

use crate::adw::prelude::*;
use crate::adw::subclass::prelude::*;
use crate::console::{Console, FdSource};
use crate::hypervisor::{Change, Hypervisor, MachineInfo, MachineState, Result};
use crate::machine::Machine;
use crate::window::MachinesWindow;
use crate::{adw, details, dialogs, glib, gtk, keymap};

/// How close to the top edge the pointer has to come, in fullscreen, to bring back the
/// header bar.
const REVEAL_EDGE: f64 = 4.0;

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
        pub view_stack: TemplateChild<adw::ViewStack>,
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
        pub console_stack: TemplateChild<gtk::Stack>,
        #[template_child]
        pub console: TemplateChild<Console>,
        #[template_child]
        pub console_message: TemplateChild<adw::StatusPage>,
        #[template_child]
        pub console_button: TemplateChild<gtk::Button>,
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

            let view = obj.downgrade();
            reveal_at_top_edge(&self.toolbar, move || {
                view.upgrade().is_some_and(|v| v.imp().fullscreen.get())
            });
            // Not an action name on the button: the button goes with the display when that
            // moves to a window of its own, out of reach of the view's actions.
            self.console_button.connect_clicked(glib::clone!(
                #[weak(rename_to = view)]
                obj,
                move |_| {
                    let action = view.imp().console_action.borrow().clone();
                    let _ = view.activate_action(&action, None);
                }
            ));

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

/// While `hidden` says so, as in fullscreen, `toolbar`'s header bar stays hidden, and
/// comes back while the pointer is at the top edge or over the bar itself.
fn reveal_at_top_edge(toolbar: &adw::ToolbarView, hidden: impl Fn() -> bool + 'static) {
    let motion = gtk::EventControllerMotion::new();
    motion.set_propagation_phase(gtk::PropagationPhase::Capture);
    motion.connect_motion(glib::clone!(
        #[weak]
        toolbar,
        move |_, _, y| {
            if !hidden() {
                return;
            }
            let bar = f64::from(toolbar.top_bar_height());
            if y <= REVEAL_EDGE {
                toolbar.set_reveal_top_bars(true);
            } else if y > bar + REVEAL_EDGE {
                toolbar.set_reveal_top_bars(false);
            }
        }
    ));
    toolbar.add_controller(motion);
}

fn install_actions(klass: &mut <imp::MachineView as ObjectSubclass>::Class) {
    klass.install_action("machine.start", None, |view, _, _| {
        view.run(|hv, uuid| hv.start(uuid));
    });
    klass.install_action("machine.shut-down", None, |view, _, _| {
        view.run(|hv, uuid| hv.shut_down(uuid));
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
    klass.install_action(
        "machine.send-keys",
        Some(glib::VariantTy::STRING),
        |view, _, target| {
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
        },
    );
    klass.install_action("machine.fullscreen", None, |view, _, _| {
        if let Some(win) = view.window() {
            win.set_fullscreened(!win.is_fullscreen());
        }
    });
    klass.install_action("machine.detach-console", None, |view, _, _| {
        view.detach_console();
    });
    klass.install_action("machine.attach-console", None, |view, _, _| {
        view.attach_console();
    });
    klass.install_action("machine.delete", None, |view, _, _| view.delete());
    klass.install_action("machine.rename", None, |view, _, _| {
        dialogs::machine::rename(view);
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

    pub fn set_machine(&self, machine: Option<&Machine>) {
        let imp = self.imp();
        if imp.machine.borrow().as_ref() == machine {
            return;
        }
        if let (Some(old), Some(handler)) = (imp.machine.take(), imp.changed_handler.take()) {
            old.disconnect(handler);
        }
        self.put_back_console();
        imp.generation.set(imp.generation.get() + 1);
        imp.connecting.set(false);
        imp.console_error.take();
        imp.console.close();
        imp.shown.take();
        self.clear_details();
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

    pub fn show_console(&self) {
        self.imp().view_stack.set_visible_child_name("console");
    }

    pub fn set_fullscreen(&self, fullscreen: bool) {
        let imp = self.imp();
        imp.fullscreen.set(fullscreen);
        imp.toolbar.set_reveal_top_bars(!fullscreen);
        imp.toolbar.set_extend_content_to_top_edge(fullscreen);
        imp.fullscreen_button.set_visible(fullscreen);
        if fullscreen {
            self.show_console();
            imp.console.grab_focus();
        }
    }

    fn update(&self) {
        let imp = self.imp();
        let Some(info) = self.info() else {
            self.update_actions();
            return;
        };
        imp.title.set_title(&info.name);
        imp.title.set_subtitle(&info.state.label());
        if let Some((window, title)) = &*imp.detached.borrow() {
            window.set_title(Some(&info.name));
            title.set_title(&info.name);
            title.set_subtitle(&info.state.label());
        }
        let resumable = matches!(info.state, MachineState::Paused | MachineState::Suspended);
        imp.start_button
            .set_visible(!info.state.is_active() || resumable);
        imp.start_button.set_tooltip_text(Some(&if resumable {
            gettext("Resume")
        } else {
            gettext("Start")
        }));
        imp.power_button.set_visible(info.state.is_active());
        if !info.state.is_active() {
            imp.console_error.take();
        }
        self.update_actions();
        self.update_console(&info);
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
            details::fill(self, &info, &imp.details_start, &imp.details_end);
            imp.shown.replace(Some(info));
        }
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
        self.action_set_enabled(
            "machine.start",
            state.is_some() && !running && state != Some(MachineState::ShuttingDown),
        );
        self.action_set_enabled("machine.shut-down", running);
        self.action_set_enabled("machine.pause", running);
        self.action_set_enabled(
            "machine.resume",
            matches!(state, Some(MachineState::Paused | MachineState::Suspended)),
        );
        self.action_set_enabled("machine.reboot", running);
        self.action_set_enabled("machine.reset", active);
        self.action_set_enabled("machine.force-off", active);
        self.action_set_enabled("machine.send-keys", running);
        self.action_set_enabled("machine.usb-devices", running);
        self.action_set_enabled(
            "machine.redirect-usb",
            running && self.usb_redirection().is_some(),
        );
        self.action_set_enabled("machine.delete", state.is_some());
        let editable = info.as_ref().is_some_and(|i| i.persistent) && !active;
        self.action_set_enabled("machine.rename", editable);
        self.action_set_enabled("machine.clone", editable);
        let imp = self.imp();
        let detached = imp.detached.borrow().is_some();
        self.action_set_enabled(
            "machine.fullscreen",
            imp.console.is_open() && !detached || imp.fullscreen.get(),
        );
        self.action_set_enabled(
            "machine.detach-console",
            active && !detached && !imp.fullscreen.get(),
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
        imp.console_message.set_description(text);
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
            self.console_message(
                "system-shutdown-symbolic",
                &info.state.label(),
                None,
                Some((&gettext("_Start"), "machine.start")),
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
        let title = adw::WindowTitle::new(&info.name, &info.state.label());
        let fullscreen = gtk::Button::builder()
            .icon_name("view-fullscreen-symbolic")
            .tooltip_text(gettext("Fullscreen"))
            .build();
        let header = adw::HeaderBar::builder().title_widget(&title).build();
        header.pack_end(&fullscreen);
        let toolbar = adw::ToolbarView::builder().css_classes(["console"]).build();
        toolbar.add_top_bar(&header);
        let (width, height) = (imp.console.width(), imp.console.height());
        imp.console_bin.set_child(gtk::Widget::NONE);
        toolbar.set_content(Some(&*imp.console_stack));
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
            move |window| {
                let on = window.is_fullscreen();
                toolbar.set_reveal_top_bars(!on);
                toolbar.set_extend_content_to_top_edge(on);
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
        let weak = window.downgrade();
        reveal_at_top_edge(&toolbar, move || {
            weak.upgrade().is_some_and(|w| w.is_fullscreen())
        });
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
        imp.console_bin.set_child(Some(&*imp.console_stack));
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
        let files = info
            .config
            .as_ref()
            .map(|c| c.disk_files())
            .unwrap_or_default();
        glib::spawn_future_local(glib::clone!(
            #[weak(rename_to = view)]
            self,
            async move {
                let Some(delete_disks) = dialogs::delete::confirm(&view, &info.name, &files).await
                else {
                    return;
                };
                let uuid = info.uuid.clone();
                match win.call(move |hv| hv.delete(&uuid, delete_disks)).await {
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
