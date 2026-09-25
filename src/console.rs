//! `MachinesConsole`: a machine's VNC or SPICE display, scaled to fit, taking the keyboard
//! and pointer while it has the focus.
//!
//! The socket comes from libvirt already authenticated, so gvnc only has to speak RFB over
//! it. gvnc decodes into a buffer this widget owns; each redraw after an update copies that
//! buffer into a texture. SPICE is in [`spice`].

mod spice;

pub use spice::FdSource;

use std::cell::{Cell, RefCell};
use std::sync::OnceLock;

use gettextrs::gettext;
use gvnc::prelude::*;

use crate::adw::prelude::*;
use crate::adw::subclass::prelude::*;
use crate::glib::translate::{FromGlibPtrFull, IntoGlib, ToGlibPtr};
use crate::keymap;
use crate::{gdk, glib, gtk};
use gtk::graphene;

const BYTES_PER_PIXEL: usize = 4;

/// 32-bit little-endian xRGB: in memory B, G, R, unused, which is `B8g8r8x8`.
fn local_format() -> gvnc::PixelFormat {
    gvnc::PixelFormat::new_with(
        (255, 255, 255),
        (16, 8, 0),
        24,
        32,
        gvnc::ByteOrder::Little,
        1,
    )
    .expect("a valid pixel format")
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct Console {
        pub(super) connection: RefCell<Option<gvnc::Connection>>,
        pub(super) framebuffer: RefCell<Option<gvnc::BaseFramebuffer>>,
        pub(super) texture: RefCell<Option<gdk::Texture>>,
        pub(super) dirty: Cell<bool>,
        /// Whether the next incremental update request is already scheduled.
        pub(super) update_requested: Cell<bool>,
        pub(super) buttons: Cell<u8>,
        /// Where the pointer last was, in widget coordinates; scroll events carry none.
        pub(super) pointer: Cell<(f64, f64)>,
        /// Keys sent down and not yet up, to release when the focus leaves.
        pub(super) pressed: RefCell<Vec<(u32, u16)>>,
        /// Whether Ctrl and Alt are down with no other key, so that letting go of them
        /// gives the keyboard back.
        pub(super) release_armed: Cell<bool>,
        /// Application shortcuts put aside while the console has the keyboard.
        pub(super) accels: RefCell<Vec<(String, Vec<glib::GString>)>>,
        /// The window whose system shortcuts (Super, Alt+Tab…) go to the guest, from a click
        /// in the console until it loses the focus.
        pub(super) grabbed: RefCell<Option<gdk::Toplevel>>,
        pub(super) error: RefCell<Option<String>>,
        pub(super) spice: RefCell<spice::Spice>,
        /// Whether the texture has its first row at the bottom, as GL frames can.
        pub(super) flipped: Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Console {
        const NAME: &'static str = "MachinesConsole";
        type Type = super::Console;
        type ParentType = gtk::Widget;

        fn class_init(klass: &mut Self::Class) {
            klass.set_css_name("machines-console");
            // ARIA's role for a region that handles every key itself, which screen readers
            // pass keys through to.
            klass.set_accessible_role(gtk::AccessibleRole::Application);
        }
    }

    impl ObjectImpl for Console {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            obj.set_focusable(true);
            obj.set_focus_on_click(true);
            obj.set_overflow(gtk::Overflow::Hidden);
            obj.update_property(&[gtk::accessible::Property::Description(&gettext(
                "Takes every key for the virtual machine; press and let go of Ctrl+Alt to \
                 leave it",
            ))]);
            obj.setup_input();
        }

        fn signals() -> &'static [glib::subclass::Signal] {
            static SIGNALS: OnceLock<Vec<glib::subclass::Signal>> = OnceLock::new();
            SIGNALS.get_or_init(|| {
                vec![
                    glib::subclass::Signal::builder("connected").build(),
                    // Whether the console now has the system shortcuts too.
                    glib::subclass::Signal::builder("grab-changed")
                        .param_types([bool::static_type()])
                        .build(),
                    // The reason, empty when the machine simply went away.
                    glib::subclass::Signal::builder("disconnected")
                        .param_types([String::static_type()])
                        .build(),
                ]
            })
        }

        fn dispose(&self) {
            self.obj().close();
        }
    }

    impl WidgetImpl for Console {
        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let obj = self.obj();
            if self.dirty.replace(false) {
                let texture = if obj.spice_is_open() {
                    obj.copy_spice_surface()
                } else {
                    obj.copy_framebuffer()
                };
                self.texture.replace(texture);
                self.flipped.set(false);
            }
            let Some(texture) = self.texture.borrow().clone() else {
                return;
            };
            let (x, y, scale) = obj.placement(texture.width(), texture.height());
            let rect = graphene::Rect::new(
                x as f32,
                y as f32,
                (f64::from(texture.width()) * scale) as f32,
                (f64::from(texture.height()) * scale) as f32,
            );
            let filter = if (scale - 1.0).abs() < f64::EPSILON {
                gtk::gsk::ScalingFilter::Nearest
            } else {
                gtk::gsk::ScalingFilter::Linear
            };
            if self.flipped.get() {
                snapshot.save();
                snapshot.translate(&graphene::Point::new(0.0, rect.y() * 2.0 + rect.height()));
                snapshot.scale(1.0, -1.0);
                snapshot.append_scaled_texture(&texture, filter, &rect);
                snapshot.restore();
            } else {
                snapshot.append_scaled_texture(&texture, filter, &rect);
            }
            obj.after_spice_frame();
        }

        fn size_allocate(&self, width: i32, height: i32, baseline: i32) {
            self.parent_size_allocate(width, height, baseline);
            if self.obj().spice_is_open() {
                self.obj().resize_guest();
            }
        }

        fn measure(&self, _orientation: gtk::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            (0, 0, -1, -1)
        }
    }
}

glib::wrapper! {
    pub struct Console(ObjectSubclass<imp::Console>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Default for Console {
    fn default() -> Self {
        glib::Object::new()
    }
}

impl Console {
    /// Speak VNC over `fd`, which the console owns from here on.
    pub fn open(&self, fd: i32) {
        self.close();
        let conn = gvnc::Connection::new();
        conn.connect_vnc_auth_choose_type(|conn, types| {
            let offered: Vec<gvnc::ConnectionAuth> =
                types.iter().filter_map(|v| v.get().ok()).collect();
            let chosen = if offered.contains(&gvnc::ConnectionAuth::None) {
                gvnc::ConnectionAuth::None
            } else {
                offered
                    .first()
                    .copied()
                    .unwrap_or(gvnc::ConnectionAuth::None)
            };
            let _ = conn.set_auth_type(chosen.into_glib() as u32);
        });
        conn.connect_vnc_initialized(glib::clone!(
            #[weak(rename_to = console)]
            self,
            move |conn| console.on_initialized(conn)
        ));
        conn.connect_vnc_desktop_resize(glib::clone!(
            #[weak(rename_to = console)]
            self,
            move |conn, _, _| console.on_resize(conn)
        ));
        conn.connect_vnc_pixel_format_changed(glib::clone!(
            #[weak(rename_to = console)]
            self,
            move |conn, _| console.on_resize(conn)
        ));
        conn.connect_vnc_framebuffer_update(glib::clone!(
            #[weak(rename_to = console)]
            self,
            move |_, _, _, _, _| {
                console.imp().dirty.set(true);
                console.queue_draw();
                console.request_update();
            }
        ));
        conn.connect_vnc_cursor_changed(glib::clone!(
            #[weak(rename_to = console)]
            self,
            move |_, cursor| console.set_remote_cursor(cursor)
        ));
        conn.connect_vnc_error(glib::clone!(
            #[weak(rename_to = console)]
            self,
            move |_, message| {
                console.imp().error.replace(Some(message.to_owned()));
            }
        ));
        conn.connect_vnc_disconnected(glib::clone!(
            #[weak(rename_to = console)]
            self,
            move |conn| {
                if console.imp().connection.borrow().as_ref() != Some(conn) {
                    return;
                }
                let reason = console.imp().error.take().unwrap_or_default();
                console.close();
                console.emit_by_name::<()>("disconnected", &[&reason]);
            }
        ));
        if let Err(e) = conn.open_fd(fd) {
            self.emit_by_name::<()>("disconnected", &[&e.to_string()]);
            return;
        }
        self.imp().connection.replace(Some(conn));
    }

    pub fn close(&self) {
        let imp = self.imp();
        if let Some(conn) = imp.connection.take() {
            conn.shutdown();
        }
        self.close_spice();
        self.ungrab_shortcuts();
        imp.framebuffer.take();
        imp.texture.take();
        imp.pressed.borrow_mut().clear();
        imp.buttons.set(0);
        self.set_cursor(None);
        self.queue_draw();
    }

    pub fn is_open(&self) -> bool {
        self.imp().connection.borrow().is_some() || self.spice_is_open()
    }

    pub fn connect_connected<F: Fn(&Self) + 'static>(&self, f: F) -> glib::SignalHandlerId {
        self.connect_local("connected", false, move |args| {
            f(&args[0].get().expect("a Console"));
            None
        })
    }

    pub fn connect_grab_changed<F: Fn(&Self, bool) + 'static>(
        &self,
        f: F,
    ) -> glib::SignalHandlerId {
        self.connect_local("grab-changed", false, move |args| {
            let grabbed: bool = args[1].get().expect("a bool");
            f(&args[0].get().expect("a Console"), grabbed);
            None
        })
    }

    pub fn connect_disconnected<F: Fn(&Self, &str) + 'static>(
        &self,
        f: F,
    ) -> glib::SignalHandlerId {
        self.connect_local("disconnected", false, move |args| {
            let reason: String = args[1].get().expect("a reason");
            f(&args[0].get().expect("a Console"), &reason);
            None
        })
    }

    fn on_initialized(&self, conn: &gvnc::Connection) {
        use gvnc::ConnectionEncoding as E;
        let _ = conn.set_pixel_format(&local_format());
        self.on_resize(conn);
        // The socket is local, so plain encodings beat compressed ones.
        let encodings: Vec<i32> = [
            E::Zrle,
            E::Hextile,
            E::Rre,
            E::CopyRect,
            E::Raw,
            E::DesktopResize,
            E::ExtendedDesktopResize,
            E::Wmvi,
            E::ExtKeyEvent,
            E::LedState,
            E::PointerChange,
            E::RichCursor,
            E::Xcursor,
            E::AlphaCursor,
            E::LastRect,
        ]
        .into_iter()
        .map(|e| e.into_glib())
        .collect();
        let _ = conn.set_encodings(&encodings);
        self.emit_by_name::<()>("connected", &[]);
    }

    /// A new framebuffer for the desktop's current size and pixel format.
    fn on_resize(&self, conn: &gvnc::Connection) {
        let (Ok(width), Ok(height)) = (u16::try_from(conn.width()), u16::try_from(conn.height()))
        else {
            return;
        };
        let Some(remote) = conn.pixel_format() else {
            return;
        };
        let stride = usize::from(width) * BYTES_PER_PIXEL;
        let mut buffer = vec![0u8; stride * usize::from(height)].into_boxed_slice();
        let local = local_format();
        // SAFETY: the buffer holds `height` rows of `stride` bytes, and is attached to the
        // framebuffer below, so it lives exactly as long as the framebuffer does, however
        // long gvnc keeps its reference. Boxed, it does not move when the box does.
        let framebuffer: gvnc::BaseFramebuffer = unsafe {
            let fb = gvnc::ffi::vnc_base_framebuffer_new(
                buffer.as_mut_ptr(),
                width,
                height,
                stride as i32,
                local.to_glib_none().0,
                remote.to_glib_none().0,
            );
            gvnc::BaseFramebuffer::from_glib_full(fb)
        };
        // SAFETY: the key is private to this function and always holds a `Box<[u8]>`.
        unsafe { framebuffer.set_data("machines-buffer", buffer) };
        if conn.set_framebuffer(&framebuffer).is_err() {
            return;
        }
        let _ = conn.framebuffer_update_request(false, 0, 0, width, height);
        self.imp().framebuffer.replace(Some(framebuffer));
        self.imp().dirty.set(true);
        self.queue_draw();
    }

    /// gvnc asks for the first frame only; every later one has to be asked for. An update
    /// arrives as one signal per rectangle, so the request waits for the main loop to
    /// finish the batch.
    fn request_update(&self) {
        let imp = self.imp();
        if imp.update_requested.replace(true) {
            return;
        }
        glib::idle_add_local_once(glib::clone!(
            #[weak(rename_to = console)]
            self,
            move || {
                console.imp().update_requested.set(false);
                let fb = console.imp().framebuffer.borrow().clone();
                if let (Some(conn), Some(fb)) = (console.connection(), fb) {
                    let _ = conn.framebuffer_update_request(
                        true,
                        0,
                        0,
                        FramebufferExt::width(&fb),
                        FramebufferExt::height(&fb),
                    );
                }
            }
        ));
    }

    fn copy_framebuffer(&self) -> Option<gdk::Texture> {
        let fb = self.imp().framebuffer.borrow().clone()?;
        let bytes = glib::Bytes::from(gvnc::FramebufferManualExt::buffer(&fb));
        Some(
            gdk::MemoryTexture::new(
                i32::from(FramebufferExt::width(&fb)),
                i32::from(FramebufferExt::height(&fb)),
                gdk::MemoryFormat::B8g8r8x8,
                &bytes,
                usize::from(FramebufferExt::width(&fb)) * BYTES_PER_PIXEL,
            )
            .upcast(),
        )
    }

    /// Where a desktop of `width`×`height` goes in the widget: its offset and scale, as
    /// large as fits with the aspect ratio kept.
    fn placement(&self, width: i32, height: i32) -> (f64, f64, f64) {
        let (w, h) = (f64::from(self.width()), f64::from(self.height()));
        let (fw, fh) = (f64::from(width.max(1)), f64::from(height.max(1)));
        let scale = (w / fw).min(h / fh);
        ((w - fw * scale) / 2.0, (h - fh * scale) / 2.0, scale)
    }

    /// The desktop pixel under the widget point (`x`, `y`), clamped to the desktop.
    fn to_desktop(&self, x: f64, y: f64) -> Option<(u16, u16)> {
        let texture = self.imp().texture.borrow().clone()?;
        let (width, height) = (texture.width(), texture.height());
        let (ox, oy, scale) = self.placement(width, height);
        let clamp = |v: f64, max: i32| (v.max(0.0) as i32).min(max - 1).max(0) as u16;
        Some((
            clamp((x - ox) / scale, width),
            clamp((y - oy) / scale, height),
        ))
    }

    fn set_remote_cursor(&self, cursor: Option<&gvnc::Cursor>) {
        let Some(cursor) = cursor.filter(|c| c.width() > 0 && c.height() > 0) else {
            self.set_cursor(gdk::Cursor::from_name("none", None).as_ref());
            return;
        };
        let (width, height) = (usize::from(cursor.width()), usize::from(cursor.height()));
        // SAFETY: gvnc keeps width × height RGBA pixels for the cursor's lifetime.
        // `Cursor::data` computes that length in u16 and overflows for 128×128 cursors.
        let data = unsafe {
            let ptr = gvnc::ffi::vnc_cursor_get_data(cursor.to_glib_none().0);
            std::slice::from_raw_parts(ptr, width * height * BYTES_PER_PIXEL)
        };
        let texture = gdk::MemoryTexture::new(
            width as i32,
            height as i32,
            gdk::MemoryFormat::R8g8b8a8,
            &glib::Bytes::from(data),
            width * BYTES_PER_PIXEL,
        );
        self.set_cursor(Some(&gdk::Cursor::from_texture(
            &texture,
            i32::from(cursor.hotx()),
            i32::from(cursor.hoty()),
            None,
        )));
    }

    fn connection(&self) -> Option<gvnc::Connection> {
        self.imp()
            .connection
            .borrow()
            .clone()
            .filter(|c| c.is_initialized())
    }

    fn send_pointer(&self, x: f64, y: f64) {
        self.imp().pointer.set((x, y));
        let Some((x, y)) = self.to_desktop(x, y) else {
            return;
        };
        if let Some(conn) = self.connection() {
            let _ = conn.pointer_event(self.imp().buttons.get(), x, y);
        } else {
            self.spice_pointer(x, y);
        }
    }

    fn setup_input(&self) {
        let motion = gtk::EventControllerMotion::new();
        motion.connect_motion(glib::clone!(
            #[weak(rename_to = console)]
            self,
            move |_, x, y| console.send_pointer(x, y)
        ));
        self.add_controller(motion);

        let click = gtk::GestureClick::builder().button(0).build();
        click.connect_pressed(glib::clone!(
            #[weak(rename_to = console)]
            self,
            move |gesture, _, x, y| {
                console.grab_focus();
                console.grab_shortcuts(gesture.current_event());
                let imp = console.imp();
                imp.buttons
                    .set(imp.buttons.get() | button_bit(gesture.current_button()));
                console.send_pointer(x, y);
                console.spice_button(gesture.current_button(), true);
            }
        ));
        click.connect_released(glib::clone!(
            #[weak(rename_to = console)]
            self,
            move |gesture, _, x, y| {
                let imp = console.imp();
                imp.buttons
                    .set(imp.buttons.get() & !button_bit(gesture.current_button()));
                console.send_pointer(x, y);
                console.spice_button(gesture.current_button(), false);
            }
        ));
        self.add_controller(click);

        let scroll = gtk::EventControllerScroll::new(
            gtk::EventControllerScrollFlags::BOTH_AXES | gtk::EventControllerScrollFlags::DISCRETE,
        );
        scroll.connect_scroll(glib::clone!(
            #[weak(rename_to = console)]
            self,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |_, dx, dy| {
                if console.spice_is_open() {
                    if dy != 0.0 {
                        console.spice_scroll(dy);
                    }
                    return glib::Propagation::Stop;
                }
                let Some(conn) = console.connection() else {
                    return glib::Propagation::Proceed;
                };
                let (px, py) = console.imp().pointer.get();
                let Some((x, y)) = console.to_desktop(px, py) else {
                    return glib::Propagation::Proceed;
                };
                let buttons = console.imp().buttons.get();
                let steps = [
                    (dy < 0.0, 8u8),
                    (dy > 0.0, 16),
                    (dx < 0.0, 32),
                    (dx > 0.0, 64),
                ];
                for (_, bit) in steps.into_iter().filter(|(on, _)| *on) {
                    let _ = conn.pointer_event(buttons | bit, x, y);
                    let _ = conn.pointer_event(buttons, x, y);
                }
                glib::Propagation::Stop
            }
        ));
        self.add_controller(scroll);

        let keys = gtk::EventControllerKey::new();
        keys.connect_key_pressed(glib::clone!(
            #[weak(rename_to = console)]
            self,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |_, key, code, _| {
                console.send_key(true, key.into_glib(), code);
                glib::Propagation::Stop
            }
        ));
        keys.connect_key_released(glib::clone!(
            #[weak(rename_to = console)]
            self,
            move |_, key, code, _| console.send_key(false, key.into_glib(), code)
        ));
        self.add_controller(keys);

        let focus = gtk::EventControllerFocus::new();
        focus.connect_enter(glib::clone!(
            #[weak(rename_to = console)]
            self,
            move |_| {
                console.hold_shortcuts(true);
                console.offer_pending_clipboard();
            }
        ));
        focus.connect_leave(glib::clone!(
            #[weak(rename_to = console)]
            self,
            move |_| {
                console.release_keys();
                console.hold_shortcuts(false);
                console.ungrab_shortcuts();
            }
        ));
        self.add_controller(focus);
    }

    fn send_key(&self, down: bool, keysym: u32, hardware_code: u32) {
        // X11 and Wayland both number keys as evdev + 8.
        let scancode = keymap::qnum(hardware_code.saturating_sub(8));
        {
            let mut pressed = self.imp().pressed.borrow_mut();
            if down {
                pressed.push((keysym, scancode));
            } else {
                pressed.retain(|&(_, s)| s != scancode);
            }
        }
        self.key_event(down, keysym, scancode);
        self.follow_release_chord(down, keysym);
    }

    /// Ctrl and Alt pressed together, with no other key, and let go: the console gives up
    /// the keyboard, which it otherwise keeps for the guest, Tab included.
    fn follow_release_chord(&self, down: bool, keysym: u32) {
        let imp = self.imp();
        if down {
            let pressed = imp.pressed.borrow();
            let has = |wanted: fn(u32) -> bool| pressed.iter().any(|&(k, _)| wanted(k));
            let other = has(|k| !is_ctrl(k) && !is_alt(k));
            imp.release_armed
                .set(!other && (imp.release_armed.get() || has(is_ctrl) && has(is_alt)));
        } else if imp.release_armed.replace(false) && (is_ctrl(keysym) || is_alt(keysym)) {
            self.release_keys();
            if let Some(root) = self.root() {
                root.set_focus(gtk::Widget::NONE);
            }
        }
    }

    fn key_event(&self, down: bool, keysym: u32, scancode: u16) {
        if let Some(conn) = self.connection() {
            let _ = conn.key_event(down, keysym, scancode);
        } else if scancode != 0 {
            self.spice_key(down, scancode);
        }
    }

    fn release_keys(&self) {
        let pressed = self.imp().pressed.take();
        for (keysym, scancode) in pressed.into_iter().rev() {
            self.key_event(false, keysym, scancode);
        }
    }

    /// Keys typed into the machine are for the machine: while the console has the focus,
    /// the application's shortcuts (Ctrl+Q, Ctrl+N…) are taken off and put back after.
    fn hold_shortcuts(&self, hold: bool) {
        let Some(app) = self
            .root()
            .and_downcast::<gtk::Window>()
            .and_then(|w| w.application())
        else {
            return;
        };
        let mut held = self.imp().accels.borrow_mut();
        if hold {
            for action in app.list_action_descriptions() {
                let accels = app.accels_for_action(&action);
                if !accels.is_empty() {
                    app.set_accels_for_action(&action, &[]);
                    held.push((action.to_string(), accels.to_vec()));
                }
            }
        } else {
            for (action, accels) in held.drain(..) {
                let accels: Vec<&str> = accels.iter().map(|a| a.as_str()).collect();
                app.set_accels_for_action(&action, &accels);
            }
        }
    }

    /// Hand the guest the shortcuts the desktop would otherwise take. The desktop may ask
    /// the user first.
    fn grab_shortcuts(&self, event: Option<gdk::Event>) {
        if !self.is_open() || self.imp().grabbed.borrow().is_some() {
            return;
        }
        let Some(toplevel) = self
            .native()
            .and_then(|n| n.surface())
            .and_downcast::<gdk::Toplevel>()
        else {
            return;
        };
        toplevel.inhibit_system_shortcuts(event);
        self.imp().grabbed.replace(Some(toplevel));
        self.emit_by_name::<()>("grab-changed", &[&true]);
    }

    fn ungrab_shortcuts(&self) {
        if let Some(toplevel) = self.imp().grabbed.take() {
            toplevel.restore_system_shortcuts();
            self.emit_by_name::<()>("grab-changed", &[&false]);
        }
    }
}

fn is_ctrl(keysym: u32) -> bool {
    [gdk::Key::Control_L, gdk::Key::Control_R]
        .iter()
        .any(|k| k.into_glib() == keysym)
}

fn is_alt(keysym: u32) -> bool {
    [
        gdk::Key::Alt_L,
        gdk::Key::Alt_R,
        gdk::Key::Meta_L,
        gdk::Key::Meta_R,
    ]
    .iter()
    .any(|k| k.into_glib() == keysym)
}

fn button_bit(button: u32) -> u8 {
    match button {
        1 => 1,
        2 => 2,
        3 => 4,
        _ => 0,
    }
}
