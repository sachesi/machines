//! The console's SPICE side. Every channel of a SPICE session is a socket of its own, and
//! each comes from libvirt like the first, through [`FdSource`].
//!
//! The screen arrives either as a surface in memory, copied into a texture per redraw
//! like VNC's, or, with 3D acceleration, as a dmabuf the host GPU rendered into, which GTK
//! draws without a copy.

use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::rc::Rc;
use std::time::Duration;

use spice_client_glib as spice;
use spice_client_glib::prelude::*;

use super::{BYTES_PER_PIXEL, Console, button_bit};
use crate::adw::prelude::*;
use crate::adw::subclass::prelude::*;
use crate::{gdk, glib};

/// Asks libvirt for another socket to the machine's display, and hands it to the callback,
/// or nothing if there is none to be had.
pub type FdSource = Rc<dyn Fn(Box<dyn FnOnce(Option<i32>)>)>;

const MOUSE_MODE_CLIENT: i32 = 2;
const MOUSE_BUTTON_UP: i32 = 4;
const MOUSE_BUTTON_DOWN: i32 = 5;
/// How long the window's size has to hold before the guest is asked to take it.
const RESIZE_SETTLE: Duration = Duration::from_millis(300);
/// `DRM_FORMAT_MOD_INVALID`: the buffer's layout is whatever the driver made it.
const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

#[derive(Default)]
pub(super) struct Spice {
    pub session: Option<spice::Session>,
    pub main: Option<spice::MainChannel>,
    pub display: Option<spice::DisplayChannel>,
    pub inputs: Option<spice::InputsChannel>,
    pub cursor: Option<spice::CursorChannel>,
    pub audio: Option<spice::Audio>,
    /// A 3D frame drawn that QEMU waits to hear is done with.
    pub draw_pending: bool,
    pub resize: Option<glib::SourceId>,
    pub error: Option<String>,
}

impl Console {
    fn spice_state(&self) -> std::cell::RefMut<'_, Spice> {
        self.imp().spice.borrow_mut()
    }

    /// Speak SPICE over `fd`, and over the sockets `more` gives for its other channels.
    pub fn open_spice(&self, fd: i32, more: FdSource) {
        self.close();
        let session = spice::Session::new();
        session.set_enable_audio(true);
        session.set_enable_usbredir(false);
        session.set_gl_scanout(true);
        session.connect_channel_new(glib::clone!(
            #[weak(rename_to = console)]
            self,
            move |session, channel| console.on_channel(session, channel, &more)
        ));
        session.connect_disconnected(glib::clone!(
            #[weak(rename_to = console)]
            self,
            move |session| {
                let current = console.imp().spice.borrow().session.clone();
                if current.as_ref() == Some(session) {
                    let reason = console.spice_state().error.take().unwrap_or_default();
                    console.close();
                    console.emit_by_name::<()>("disconnected", &[&reason]);
                }
            }
        ));
        self.spice_state().session = Some(session.clone());
        self.spice_state().audio = spice::Audio::get(&session, None);
        if !session.open_fd(fd) {
            self.close();
            self.emit_by_name::<()>("disconnected", &[&String::new()]);
        }
    }

    pub(super) fn close_spice(&self) {
        let state = self.imp().spice.take();
        if let Some(source) = state.resize {
            source.remove();
        }
        if let (true, Some(display)) = (state.draw_pending, &state.display) {
            display.gl_draw_done();
        }
        if let Some(session) = state.session {
            session.disconnect();
        }
    }

    pub(super) fn spice_is_open(&self) -> bool {
        self.imp().spice.borrow().session.is_some()
    }

    fn on_channel(&self, session: &spice::Session, channel: &spice::Channel, more: &FdSource) {
        if self.imp().spice.borrow().session.as_ref() != Some(session) {
            return;
        }
        let more = more.clone();
        channel.connect_open_fd(glib::clone!(
            #[weak(rename_to = console)]
            self,
            move |channel, _tls| {
                let channel = channel.clone();
                let session = console.imp().spice.borrow().session.clone();
                more(Box::new(glib::clone!(
                    #[weak]
                    console,
                    move |fd| {
                        let Some(fd) = fd else {
                            return;
                        };
                        let current = console.imp().spice.borrow().session.clone();
                        if current.is_some() && current == session {
                            channel.open_fd(fd);
                        } else {
                            // SAFETY: libvirt handed this descriptor over and nothing else
                            // has it.
                            drop(unsafe { OwnedFd::from_raw_fd(fd) });
                        }
                    }
                )));
            }
        ));
        channel.connect_channel_event(glib::clone!(
            #[weak(rename_to = console)]
            self,
            move |channel, event| {
                let failed = matches!(
                    event,
                    spice::ChannelEvent::ErrorConnect
                        | spice::ChannelEvent::ErrorTls
                        | spice::ChannelEvent::ErrorLink
                        | spice::ChannelEvent::ErrorAuth
                        | spice::ChannelEvent::ErrorIo
                );
                if failed && let Some(error) = channel.error() {
                    console.spice_state().error = Some(error.message().to_owned());
                }
                // Without the main channel there is no session, whatever the others do.
                if (failed || event == spice::ChannelEvent::Closed)
                    && channel.is::<spice::MainChannel>()
                    && let Some(session) = channel.spice_session()
                {
                    session.disconnect();
                }
            }
        ));

        if let Some(main) = channel.downcast_ref::<spice::MainChannel>() {
            main.connect_main_mouse_update(|main| {
                if main.mouse_mode() != MOUSE_MODE_CLIENT {
                    main.request_mouse_mode(MOUSE_MODE_CLIENT);
                }
            });
            main.connect_agent_connected_notify(glib::clone!(
                #[weak(rename_to = console)]
                self,
                move |_| console.resize_guest()
            ));
            self.spice_state().main = Some(main.clone());
        } else if let Some(display) = channel.downcast_ref::<spice::DisplayChannel>() {
            if channel.channel_id() != 0 {
                return;
            }
            display.connect_display_primary_create(glib::clone!(
                #[weak(rename_to = console)]
                self,
                move |_| {
                    console.imp().dirty.set(true);
                    console.queue_draw();
                    console.emit_by_name::<()>("connected", &[]);
                }
            ));
            display.connect_display_invalidate(glib::clone!(
                #[weak(rename_to = console)]
                self,
                move |_, _, _, _, _| {
                    console.imp().dirty.set(true);
                    console.queue_draw();
                }
            ));
            display.connect_display_primary_destroy(glib::clone!(
                #[weak(rename_to = console)]
                self,
                move |_| {
                    console.imp().texture.take();
                    console.queue_draw();
                }
            ));
            display.connect_gl_draw(glib::clone!(
                #[weak(rename_to = console)]
                self,
                move |display, _, _, _, _| console.on_gl_draw(display)
            ));
            ChannelExt::connect(channel);
            self.spice_state().display = Some(display.clone());
        } else if let Some(inputs) = channel.downcast_ref::<spice::InputsChannel>() {
            ChannelExt::connect(channel);
            self.spice_state().inputs = Some(inputs.clone());
        } else if let Some(cursor) = channel.downcast_ref::<spice::CursorChannel>() {
            cursor.connect_cursor_notify(glib::clone!(
                #[weak(rename_to = console)]
                self,
                move |cursor| console.set_spice_cursor(cursor.cursor())
            ));
            cursor.connect_cursor_hide(glib::clone!(
                #[weak(rename_to = console)]
                self,
                move |_| console.set_cursor(gdk::Cursor::from_name("none", None).as_ref())
            ));
            cursor.connect_cursor_reset(glib::clone!(
                #[weak(rename_to = console)]
                self,
                move |_| console.set_cursor(None)
            ));
            ChannelExt::connect(channel);
            self.spice_state().cursor = Some(cursor.clone());
        } else if channel.is::<spice::PlaybackChannel>() {
            ChannelExt::connect(channel);
        }
    }

    /// The surface in memory, as a texture.
    pub(super) fn copy_spice_surface(&self) -> Option<gdk::Texture> {
        let display = self.imp().spice.borrow().display.clone()?;
        let primary = display.primary(0)?;
        let format = match primary.format() {
            Ok(spice::SurfaceFormat::_32XRGB) => gdk::MemoryFormat::B8g8r8x8,
            Ok(spice::SurfaceFormat::_32ARGB) => gdk::MemoryFormat::B8g8r8a8Premultiplied,
            _ => return None,
        };
        let (width, height) = (primary.width(), primary.height());
        if width == 0 || height == 0 || primary.stride() < width * BYTES_PER_PIXEL {
            return None;
        }
        Some(
            gdk::MemoryTexture::new(
                i32::try_from(width).ok()?,
                i32::try_from(height).ok()?,
                format,
                &glib::Bytes::from(primary.data()),
                primary.stride(),
            )
            .upcast(),
        )
    }

    /// A frame the host GPU rendered: shown as it is, and QEMU told once it is on screen.
    fn on_gl_draw(&self, display: &spice::DisplayChannel) {
        let texture = display.gl_scanout().and_then(|scanout| {
            // SAFETY: the scanout's descriptor stays open while the channel has it, which
            // is at least until this call returns.
            let fd = unsafe { BorrowedFd::borrow_raw(scanout.fd()) }
                .try_clone_to_owned()
                .ok()?;
            let raw = fd.as_raw_fd();
            // SAFETY: `fd` is this texture's own duplicate of the buffer, and is closed
            // only when GTK releases the texture.
            let texture = unsafe {
                gdk::DmabufTextureBuilder::new()
                    .set_display(&self.display())
                    .set_width(scanout.width())
                    .set_height(scanout.height())
                    .set_fourcc(scanout.format())
                    .set_modifier(DRM_FORMAT_MOD_INVALID)
                    .set_n_planes(1)
                    .set_fd(0, raw)
                    .set_stride(0, scanout.stride())
                    .set_offset(0, 0)
                    .build_with_release_func(move || drop(fd))
            };
            // `y0top` is for a GL view, which puts a texture's first row at the bottom;
            // GTK puts it at the top, so the frame turns over exactly when it is set.
            Some((texture.ok()?, scanout.y0_top()))
        });
        let imp = self.imp();
        match texture {
            Some((texture, flipped)) => {
                imp.texture.replace(Some(texture));
                imp.flipped.set(flipped);
                imp.dirty.set(false);
                self.spice_state().draw_pending = true;
                self.queue_draw();
                self.emit_by_name::<()>("connected", &[]);
            }
            None => display.gl_draw_done(),
        }
    }

    /// Once a 3D frame is drawn, QEMU may render the next into the same buffer.
    pub(super) fn after_spice_frame(&self) {
        if !std::mem::take(&mut self.spice_state().draw_pending) {
            return;
        }
        glib::idle_add_local_once(glib::clone!(
            #[weak(rename_to = console)]
            self,
            move || {
                let display = console.imp().spice.borrow().display.clone();
                if let Some(display) = display {
                    display.gl_draw_done();
                }
            }
        ));
    }

    fn set_spice_cursor(&self, shape: Option<spice::CursorShape>) {
        let Some(shape) = shape else {
            self.set_cursor(None);
            return;
        };
        let (Ok(width), Ok(height), Ok(data)) = (
            usize::try_from(shape.width()),
            usize::try_from(shape.height()),
            shape.data(),
        ) else {
            return;
        };
        if width == 0 || height == 0 || data.len() < width * height * BYTES_PER_PIXEL {
            return;
        }
        let texture = gdk::MemoryTexture::new(
            shape.width(),
            shape.height(),
            gdk::MemoryFormat::B8g8r8a8,
            &glib::Bytes::from(data),
            width * BYTES_PER_PIXEL,
        );
        self.set_cursor(Some(&gdk::Cursor::from_texture(
            &texture,
            shape.hot_x(),
            shape.hot_y(),
            None,
        )));
    }

    fn spice_buttons(&self) -> i32 {
        i32::from(self.imp().buttons.get())
    }

    pub(super) fn spice_pointer(&self, x: u16, y: u16) {
        let inputs = self.imp().spice.borrow().inputs.clone();
        if let Some(inputs) = inputs {
            inputs.position(i32::from(x), i32::from(y), 0, self.spice_buttons());
        }
    }

    /// Press or release GTK button `button`; the button mask is already updated.
    pub(super) fn spice_button(&self, button: u32, down: bool) {
        let inputs = self.imp().spice.borrow().inputs.clone();
        let (Some(inputs), true) = (inputs, button_bit(button) != 0) else {
            return;
        };
        // SPICE numbers left, middle and right 1, 2 and 3, as GTK does.
        let button = button as i32;
        if down {
            inputs.button_press(button, self.spice_buttons());
        } else {
            inputs.button_release(button, self.spice_buttons());
        }
    }

    pub(super) fn spice_scroll(&self, dy: f64) {
        let inputs = self.imp().spice.borrow().inputs.clone();
        let Some(inputs) = inputs else {
            return;
        };
        let button = if dy < 0.0 {
            MOUSE_BUTTON_UP
        } else {
            MOUSE_BUTTON_DOWN
        };
        let buttons = self.spice_buttons();
        inputs.button_press(button, buttons);
        inputs.button_release(button, buttons);
    }

    /// Press or release the key of QEMU number `qnum`.
    pub(super) fn spice_key(&self, down: bool, qnum: u16) {
        let inputs = self.imp().spice.borrow().inputs.clone();
        let Some(inputs) = inputs else {
            return;
        };
        // QEMU marks the keys set 1 prefixes with 0xe0 by their top bit; SPICE by 0x100.
        let scancode = if qnum & 0x80 != 0 {
            0x100 | u32::from(qnum & 0x7f)
        } else {
            u32::from(qnum)
        };
        if down {
            inputs.key_press(scancode);
        } else {
            inputs.key_release(scancode);
        }
    }

    /// Ask the guest, through its agent, to make its screen the console's size, once the
    /// size has settled.
    pub(super) fn resize_guest(&self) {
        if let Some(source) = self.spice_state().resize.take() {
            source.remove();
        }
        let source = glib::timeout_add_local_once(
            RESIZE_SETTLE,
            glib::clone!(
                #[weak(rename_to = console)]
                self,
                move || {
                    console.spice_state().resize = None;
                    let main = console.imp().spice.borrow().main.clone();
                    let Some(main) = main.filter(|m| m.is_agent_connected()) else {
                        return;
                    };
                    let scale = console.scale_factor();
                    let (width, height) = (console.width() * scale, console.height() * scale);
                    if width > 0 && height > 0 {
                        main.update_display_enabled(0, true, false);
                        main.update_display(0, 0, 0, width, height, true);
                    }
                }
            ),
        );
        self.spice_state().resize = Some(source);
    }
}
