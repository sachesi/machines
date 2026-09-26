//! The console's Looking Glass side: the guest's screen as the Looking Glass host
//! application shares it through a kvmfr device, shown in place of the SPICE display for
//! as long as it comes. The keyboard and mouse still go over SPICE.

use crate::adw::prelude::*;
use crate::adw::subclass::prelude::*;
use crate::gtk::cairo;
use crate::looking_glass::{Format, Frame, Reader, Rect, Shape, Status};
use crate::{gdk, glib};

use super::Console;

#[derive(Default)]
pub(super) struct LookingGlass {
    reader: Option<Reader>,
    status: Option<Status>,
    texture: Option<gdk::Texture>,
    /// The size of the guest's screen, which its pointer moves in.
    screen: (i32, i32),
    /// Where the guest's pointer is, as the guest last said, moved by what was sent it
    /// since.
    pointer: Option<(i32, i32)>,
    cursor: Option<gdk::Cursor>,
    visible: bool,
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
        self.imp().looking_glass.take();
    }

    /// How the Looking Glass host application is doing, while the console watches for it.
    pub fn looking_glass_status(&self) -> Option<Status> {
        self.imp().looking_glass.borrow().status.clone()
    }

    pub(super) fn watches_looking_glass(&self) -> bool {
        self.imp().looking_glass.borrow().reader.is_some()
    }

    /// The guest's screen as Looking Glass shows it, and the size of the screen the
    /// pointer moves in.
    pub(super) fn looking_glass_screen(&self) -> Option<(gdk::Texture, (i32, i32))> {
        let lg = self.imp().looking_glass.borrow();
        Some((lg.texture.clone()?, lg.screen))
    }

    /// How far the guest's pointer is from (`x`, `y`), where Looking Glass says where it
    /// is, counting it as moved there.
    pub(super) fn looking_glass_pointer_to(&self, x: i32, y: i32) -> Option<(i32, i32)> {
        let mut lg = self.imp().looking_glass.borrow_mut();
        let (gx, gy) = lg.pointer?;
        lg.pointer = Some((x, y));
        Some((x - gx, y - gy))
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
            let before = lg.texture.take();
            lg.texture = Some(frame_texture(frame, update.damage, before));
        }
        if let Some(shape) = update.shape {
            lg.cursor = Some(shape_cursor(&shape));
        }
        if let Some(visible) = update.visible {
            lg.visible = visible;
        }
        if let Some(position) = update.position {
            lg.pointer = Some(position);
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
        let cursor = match (showing, lg.visible) {
            (true, true) => lg.cursor.clone(),
            (true, false) => gdk::Cursor::from_name("none", None),
            (false, _) => None,
        };
        drop(lg);
        if showing || status.is_some() {
            self.set_cursor(cursor.as_ref());
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
            .set_update_region(Some(&region));
    }
    builder.build()
}

fn shape_cursor(shape: &Shape) -> gdk::Cursor {
    let texture = gdk::MemoryTexture::new(
        shape.width as i32,
        shape.height as i32,
        gdk::MemoryFormat::B8g8r8a8Premultiplied,
        &glib::Bytes::from(&shape.pixels),
        shape.width as usize * 4,
    );
    gdk::Cursor::from_texture(
        &texture,
        shape.hotspot.0.min(shape.width as i32 - 1),
        shape.hotspot.1.min(shape.height as i32 - 1),
        None,
    )
}
