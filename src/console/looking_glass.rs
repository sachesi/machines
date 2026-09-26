//! The console's Looking Glass side: the guest's screen as the Looking Glass host
//! application shares it through a kvmfr device, shown in place of the SPICE display for
//! as long as it comes. The keyboard and mouse still go over SPICE.
//!
//! Scroll Lock does what it does in the Looking Glass client: a tap holds the pointer for
//! the guest or lets go of it, held down it shows the keys that go with it, and with
//! another key it does what that key is for there.

use std::time::Duration;

use gettextrs::gettext;

use crate::adw::prelude::*;
use crate::adw::subclass::prelude::*;
use crate::gtk::{cairo, graphene};
use crate::keymap;
use crate::looking_glass::{Format, Frame, Reader, Rect, Shape, Status};
use crate::{gdk, glib, gtk};

use super::Console;
use super::pointer_lock::PointerLock;

/// How long Scroll Lock is held before the keys that go with it are shown, as long as
/// the Looking Glass client waits.
const KEYS_DELAY: Duration = Duration::from_millis(200);
/// How far the pointer's sensitivity goes either way from 0, as in Looking Glass, where
/// each step is a tenth.
const SENSITIVITY_STEPS: i32 = 9;

#[derive(Default)]
pub(super) struct LookingGlass {
    reader: Option<Reader>,
    status: Option<Status>,
    texture: Option<gdk::Texture>,
    /// The size of the guest's screen, which its pointer moves in.
    screen: (i32, i32),
    /// How many quarter turns clockwise the frame is shown turned, as the guest's screen
    /// is, and as the user turned it on top of that.
    frame_turns: u32,
    turns: u32,
    /// Where the guest's pointer is, as the guest last said, moved by what was sent it
    /// since.
    pointer: Option<(i32, i32)>,
    cursor: Option<gdk::Cursor>,
    /// The guest's pointer and its hotspot, drawn over the screen while the console
    /// holds the pointer.
    shape: Option<(gdk::Texture, (i32, i32))>,
    visible: bool,
    /// Whether the host puts the guest's pointer where the console asks.
    places_pointer: bool,
    /// The pointer, while the console holds it for the guest.
    held: Option<PointerLock>,
    /// What the pointer moved that is too little yet to send the guest.
    remainder: (f64, f64),
    sensitivity: i32,
    escape: Option<Escape>,
    /// The keys pressed with Scroll Lock, which the guest is not to see let go either.
    swallowed: Vec<u32>,
}

/// Scroll Lock, while it is down.
struct Escape {
    /// Whether another key went with it, so that letting go of it does nothing more.
    used: bool,
    /// Whether the keys that go with it are shown.
    shown: bool,
    /// What shows them, until it does.
    timeout: Option<glib::SourceId>,
}

impl Console {
    /// Show the guest's screen from the kvmfr device at `path` whenever the Looking Glass
    /// host application in machine `uuid` shares it there.
    pub fn watch_looking_glass(&self, path: &str, uuid: &str) {
        let console = glib::SendWeakRef::from(self.downgrade());
        let context = glib::MainContext::default();
        let reader = Reader::start(path, uuid, move || {
            let console = console.clone();
            context.invoke(move || {
                if let Some(console) = console.upgrade() {
                    console.on_looking_glass();
                }
            });
        });
        self.imp().looking_glass.borrow_mut().reader = Some(reader);
        self.on_looking_glass();
    }

    pub(super) fn close_looking_glass(&self) {
        self.release_looking_glass();
        self.imp().looking_glass.take();
    }

    /// How the Looking Glass host application is doing, while the console watches for it.
    pub fn looking_glass_status(&self) -> Option<Status> {
        self.imp().looking_glass.borrow().status.clone()
    }

    /// The guest's screen as Looking Glass last showed it, while it shows it.
    pub fn looking_glass_frame(&self) -> Option<gdk::Texture> {
        self.imp().looking_glass.borrow().texture.clone()
    }

    pub(super) fn watches_looking_glass(&self) -> bool {
        self.imp().looking_glass.borrow().reader.is_some()
    }

    /// The guest's screen as Looking Glass shows it, the size of the screen the pointer
    /// moves in, and how many quarter turns clockwise it is shown turned.
    pub(super) fn looking_glass_screen(&self) -> Option<(gdk::Texture, (i32, i32), u32)> {
        let lg = self.imp().looking_glass.borrow();
        let turns = (lg.frame_turns + lg.turns) % 4;
        Some((lg.texture.clone()?, lg.screen, turns))
    }

    /// How far the guest's pointer is from (`x`, `y`), where Looking Glass says where it
    /// is, counting it as moved there.
    pub(super) fn looking_glass_pointer_to(&self, x: i32, y: i32) -> Option<(i32, i32)> {
        let mut lg = self.imp().looking_glass.borrow_mut();
        let (gx, gy) = lg.pointer?;
        lg.pointer = Some((x, y));
        Some((x - gx, y - gy))
    }

    /// Have the host put the guest's pointer at (`x`, `y`), where it can, while it shows
    /// the screen; whether it does.
    pub(super) fn place_looking_glass_pointer(&self, x: i32, y: i32) -> bool {
        let lg = self.imp().looking_glass.borrow();
        let places = lg.places_pointer && lg.texture.is_some();
        if let (true, Some(reader)) = (places, &lg.reader) {
            reader.place(x, y);
        }
        places
    }

    /// Whether SPICE is to be in its server mouse mode, where the guest's mouse moves only
    /// by as much as it is told: while the console holds the pointer, and while the host
    /// puts the guest's pointer where the pointer is, as the agent of the client mode
    /// would put it back where it last had it with every click.
    pub(super) fn looking_glass_wants_server_mouse(&self) -> bool {
        let lg = self.imp().looking_glass.borrow();
        lg.held.is_some() || lg.places_pointer && lg.texture.is_some()
    }

    pub(super) fn holds_looking_glass_pointer(&self) -> bool {
        self.imp().looking_glass.borrow().held.is_some()
    }

    /// Let go of the pointer, and of Scroll Lock, as the console loses the keyboard.
    pub(super) fn release_looking_glass(&self) {
        self.release_pointer();
        self.end_escape(false);
        self.imp().looking_glass.borrow_mut().swallowed.clear();
    }

    /// Draw the guest's screen, turned as it is to be shown, and its pointer while the
    /// console holds it; whether Looking Glass shows one.
    pub(super) fn snapshot_looking_glass(&self, snapshot: &gtk::Snapshot) -> bool {
        let Some((texture, (width, height), turns)) = self.looking_glass_screen() else {
            return false;
        };
        let (w, h) = (texture.width(), texture.height());
        let (shown_w, shown_h) = if turns % 2 == 1 { (h, w) } else { (w, h) };
        let (x, y, scale) = self.placement(shown_w, shown_h);
        let filter = if (scale - 1.0).abs() < f64::EPSILON {
            gtk::gsk::ScalingFilter::Nearest
        } else {
            gtk::gsk::ScalingFilter::Linear
        };
        let scale = scale as f32;
        let (w, h) = (w as f32 * scale, h as f32 * scale);
        snapshot.save();
        snapshot.translate(&graphene::Point::new(
            x as f32 + shown_w as f32 * scale / 2.0,
            y as f32 + shown_h as f32 * scale / 2.0,
        ));
        if turns != 0 {
            snapshot.rotate(90.0 * turns as f32);
        }
        snapshot.append_scaled_texture(
            &texture,
            filter,
            &graphene::Rect::new(-w / 2.0, -h / 2.0, w, h),
        );
        let lg = self.imp().looking_glass.borrow();
        if let (true, true, Some((shape, (hx, hy))), Some((px, py))) =
            (lg.held.is_some(), lg.visible, &lg.shape, lg.pointer)
        {
            // From the guest's screen to the frame, which may be smaller, then as shown.
            let kx = w / width.max(1) as f32;
            let ky = h / height.max(1) as f32;
            snapshot.append_scaled_texture(
                shape,
                filter,
                &graphene::Rect::new(
                    -w / 2.0 + (px - hx) as f32 * kx,
                    -h / 2.0 + (py - hy) as f32 * ky,
                    shape.width() as f32 * kx,
                    shape.height() as f32 * ky,
                ),
            );
        }
        snapshot.restore();
        true
    }

    /// Take the key of evdev code `code`, going `down` or up, if it is Scroll Lock or goes
    /// with it; whether it did.
    pub(super) fn looking_glass_key(&self, down: bool, code: u32) -> bool {
        if !self.watches_looking_glass() {
            return false;
        }
        let imp = self.imp();
        if code == keymap::KEY_SCROLLLOCK {
            if !down {
                self.end_escape(true);
            } else if imp.looking_glass.borrow().escape.is_none() {
                let timeout = glib::timeout_add_local_once(
                    KEYS_DELAY,
                    glib::clone!(
                        #[weak(rename_to = console)]
                        self,
                        move || console.show_escape_keys()
                    ),
                );
                imp.looking_glass.borrow_mut().escape = Some(Escape {
                    used: false,
                    shown: false,
                    timeout: Some(timeout),
                });
            }
            return true;
        }
        let mut lg = imp.looking_glass.borrow_mut();
        if let Some(escape) = &mut lg.escape {
            if down {
                escape.used = true;
                if !lg.swallowed.contains(&code) {
                    lg.swallowed.push(code);
                }
                drop(lg);
                self.looking_glass_binding(code);
            }
            return true;
        }
        match lg.swallowed.iter().position(|&c| c == code) {
            Some(i) if !down => {
                lg.swallowed.swap_remove(i);
                true
            }
            _ => false,
        }
    }

    fn show_escape_keys(&self) {
        let mut lg = self.imp().looking_glass.borrow_mut();
        let Some(escape) = &mut lg.escape else {
            return;
        };
        escape.timeout = None;
        escape.shown = true;
        drop(lg);
        self.emit_by_name::<()>("looking-glass-keys", &[&true]);
    }

    /// Scroll Lock went up, or the keyboard went away with it down. Tapped alone, it
    /// holds the pointer or lets go of it.
    fn end_escape(&self, tapped: bool) {
        let Some(escape) = self.imp().looking_glass.borrow_mut().escape.take() else {
            return;
        };
        if let Some(timeout) = escape.timeout {
            timeout.remove();
        }
        if escape.shown {
            self.emit_by_name::<()>("looking-glass-keys", &[&false]);
        } else if tapped && !escape.used {
            if self.holds_looking_glass_pointer() {
                self.release_pointer();
            } else {
                self.hold_pointer();
            }
        }
    }

    /// What the key of evdev code `code` does with Scroll Lock, as in Looking Glass.
    fn looking_glass_binding(&self, code: u32) {
        use keymap::*;
        match code {
            KEY_F => {
                if let Some(window) = self.root().and_downcast::<gtk::Window>() {
                    window.set_fullscreened(!window.is_fullscreen());
                }
            }
            KEY_R => {
                let mut lg = self.imp().looking_glass.borrow_mut();
                lg.turns = (lg.turns + 1) % 4;
                drop(lg);
                self.queue_draw();
            }
            KEY_INSERT | KEY_DELETE => {
                let mut lg = self.imp().looking_glass.borrow_mut();
                let step = if code == KEY_INSERT { 1 } else { -1 };
                lg.sensitivity =
                    (lg.sensitivity + step).clamp(-SENSITIVITY_STEPS, SENSITIVITY_STEPS);
                let sensitivity = lg.sensitivity;
                drop(lg);
                let shown = if sensitivity > 0 {
                    format!("+{sensitivity}")
                } else {
                    sensitivity.to_string()
                };
                let hint = gettext("Pointer sensitivity: {}").replace("{}", &shown);
                self.emit_by_name::<()>("hint", &[&hint]);
            }
            KEY_UP => self.tap_keys(&[KEY_VOLUMEUP]),
            KEY_DOWN => self.tap_keys(&[KEY_VOLUMEDOWN]),
            KEY_M => self.tap_keys(&[KEY_MUTE]),
            KEY_LEFTMETA | KEY_RIGHTMETA => self.tap_keys(&[code]),
            KEY_F1..=KEY_F10 | KEY_F11 | KEY_F12 => {
                self.tap_keys(&[KEY_LEFTCTRL, KEY_LEFTALT, code]);
            }
            _ => {}
        }
    }

    /// Press the keys of evdev codes `codes` in the guest, then let go of them.
    fn tap_keys(&self, codes: &[u32]) {
        for down in [true, false] {
            for &code in codes {
                self.key_event(down, 0, keymap::qnum(code));
            }
        }
    }

    /// Hold the pointer for the guest, once it is over the console, and draw the guest's
    /// own over its screen.
    fn hold_pointer(&self) {
        if self.looking_glass_screen().is_none() {
            return;
        }
        let moved = self.downgrade();
        let unlocked = moved.clone();
        let held = PointerLock::new(
            self.upcast_ref(),
            move |dx, dy| {
                if let Some(console) = moved.upgrade() {
                    console.move_held_pointer(dx, dy);
                }
            },
            move || {
                if let Some(console) = unlocked.upgrade() {
                    console.release_pointer();
                }
            },
        );
        let Some(held) = held else {
            let hint = gettext("The pointer cannot be held on this desktop");
            self.emit_by_name::<()>("hint", &[&hint]);
            return;
        };
        {
            let mut lg = self.imp().looking_glass.borrow_mut();
            lg.held = Some(held);
            lg.remainder = (0.0, 0.0);
        }
        self.grab_shortcuts(None);
        self.follow_looking_glass_cursor();
        self.follow_mouse_mode();
        self.queue_draw();
        let hint = gettext("Press Scroll Lock to release the pointer");
        self.emit_by_name::<()>("hint", &[&hint]);
    }

    fn release_pointer(&self) {
        let Some(held) = self.imp().looking_glass.borrow_mut().held.take() else {
            return;
        };
        drop(held);
        self.follow_looking_glass_cursor();
        self.follow_mouse_mode();
        self.queue_draw();
        self.emit_by_name::<()>("hint", &[&String::new()]);
    }

    /// The held pointer moved by (`dx`, `dy`): move the guest's by as much, in whole
    /// pixels, keeping what is left for the next move.
    fn move_held_pointer(&self, dx: f64, dy: f64) {
        let mut lg = self.imp().looking_glass.borrow_mut();
        let factor = f64::from(lg.sensitivity + 10) / 10.0;
        let x = lg.remainder.0 + dx * factor;
        let y = lg.remainder.1 + dy * factor;
        let (mx, my) = (x.trunc(), y.trunc());
        lg.remainder = (x - mx, y - my);
        drop(lg);
        if (mx, my) != (0.0, 0.0) {
            self.spice_motion(mx as i32, my as i32);
        }
    }

    /// The pointer over the screen: the guest's, unless the guest hides it or the console
    /// holds it, which draws it itself; the display's where Looking Glass shows nothing.
    fn follow_looking_glass_cursor(&self) {
        let lg = self.imp().looking_glass.borrow();
        let cursor = match (lg.texture.is_some(), lg.visible && lg.held.is_none()) {
            (true, true) => lg.cursor.clone(),
            (true, false) => gdk::Cursor::from_name("none", None),
            (false, _) => self.imp().remote_cursor.borrow().clone(),
        };
        drop(lg);
        self.set_cursor(cursor.as_ref());
    }

    fn on_looking_glass(&self) {
        let imp = self.imp();
        let Some(update) = imp.looking_glass.borrow().reader.as_ref().map(Reader::take) else {
            return;
        };
        let mut lg = imp.looking_glass.borrow_mut();
        let first = update.frame.is_some() && lg.texture.is_none();
        if let Some(frame) = update.frame {
            lg.screen = match frame.screen {
                (0, _) | (_, 0) => (frame.width as i32, frame.height as i32),
                (width, height) => (width as i32, height as i32),
            };
            lg.frame_turns = frame.turns;
            let before = lg.texture.take();
            lg.texture = Some(frame_texture(frame, update.damage, before));
        }
        if let Some(shape) = update.shape {
            let texture = shape_texture(&shape);
            let hotspot = (
                shape.hotspot.0.min(shape.width as i32 - 1),
                shape.hotspot.1.min(shape.height as i32 - 1),
            );
            lg.cursor = Some(gdk::Cursor::from_texture(
                &texture, hotspot.0, hotspot.1, None,
            ));
            lg.shape = Some((texture, hotspot));
        }
        if let Some(visible) = update.visible {
            lg.visible = visible;
        }
        if let Some(position) = update.position {
            lg.pointer = Some(position);
        }
        if let Some(places_pointer) = update.places_pointer {
            lg.places_pointer = places_pointer;
        }
        let status = update.status;
        if let Some(status) = &status {
            if *status != Status::Showing {
                lg.texture = None;
                lg.pointer = None;
            }
            lg.status = Some(status.clone());
        }
        let showing = lg.texture.is_some();
        drop(lg);
        if !showing {
            self.release_pointer();
        }
        if showing || status.is_some() {
            self.follow_looking_glass_cursor();
        }
        if status.is_some() || update.places_pointer.is_some() {
            self.follow_mouse_mode();
        }
        if status.is_some() && !showing {
            // What the SPICE display has, if anything, shows again.
            self.invalidate(None);
        }
        self.queue_draw();
        if status.is_some() {
            self.emit_by_name::<()>("looking-glass-changed", &[]);
        }
        if first {
            self.emit_by_name::<()>("connected", &[]);
        }
    }
}

fn frame_texture(
    frame: Frame,
    damage: Option<Vec<Rect>>,
    before: Option<gdk::Texture>,
) -> gdk::Texture {
    let (format, color_state) = match frame.format {
        Format::Bgrx => (gdk::MemoryFormat::B8g8r8x8, None),
        Format::Rgbx => (gdk::MemoryFormat::R8g8b8x8, None),
        Format::Bgr => (gdk::MemoryFormat::B8g8r8, None),
        Format::Rgb => (gdk::MemoryFormat::R8g8b8, None),
        Format::Rgb16 => (gdk::MemoryFormat::R16g16b16, None),
        Format::Rgb16Pq => (
            gdk::MemoryFormat::R16g16b16,
            Some(gdk::ColorState::rec2100_pq()),
        ),
        Format::Rgba16Float => (
            gdk::MemoryFormat::R16g16b16a16Float,
            Some(gdk::ColorState::srgb_linear()),
        ),
    };
    let (width, height) = (frame.width as i32, frame.height as i32);
    let mut builder = gdk::MemoryTextureBuilder::new()
        .set_bytes(Some(&glib::Bytes::from_owned(frame.pixels)))
        .set_width(width)
        .set_height(height)
        .set_format(format)
        .set_stride(frame.stride);
    if let Some(color_state) = &color_state {
        builder = builder.set_color_state(color_state);
    }
    let before =
        before.filter(|b| b.width() == width && b.height() == height && b.format() == format);
    if let (Some(damage), Some(before)) = (damage, before) {
        let region = cairo::Region::create();
        for rect in damage {
            let (x, y) = (rect.x.min(frame.width), rect.y.min(frame.height));
            let _ = region.union_rectangle(&cairo::RectangleInt::new(
                x as i32,
                y as i32,
                rect.width.min(frame.width - x) as i32,
                rect.height.min(frame.height - y) as i32,
            ));
        }
        builder = builder
            .set_update_texture(Some(&before))
            .set_update_region(Some(&super::with_filter_margin(&region, width, height)));
    }
    builder.build()
}

fn shape_texture(shape: &Shape) -> gdk::Texture {
    gdk::MemoryTexture::new(
        shape.width as i32,
        shape.height as i32,
        gdk::MemoryFormat::B8g8r8a8Premultiplied,
        &glib::Bytes::from(&shape.pixels),
        shape.width as usize * 4,
    )
    .upcast()
}
