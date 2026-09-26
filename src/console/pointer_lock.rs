//! The pointer held for the guest alone: the compositor locks it where it is and reports
//! how it moves instead, through Wayland's pointer constraints and relative pointer
//! protocols, which GTK does not offer.

use std::os::fd::AsRawFd;

use gdk4_wayland::prelude::*;
use gdk4_wayland::{WaylandDevice, WaylandDisplay, WaylandSurface};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_compositor::WlCompositor;
use wayland_client::protocol::wl_region::WlRegion;
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, delegate_noop};
use wayland_protocols::wp::pointer_constraints::zv1::client::zwp_locked_pointer_v1::{
    self, ZwpLockedPointerV1,
};
use wayland_protocols::wp::pointer_constraints::zv1::client::zwp_pointer_constraints_v1::{
    Lifetime, ZwpPointerConstraintsV1,
};
use wayland_protocols::wp::relative_pointer::zv1::client::zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1;
use wayland_protocols::wp::relative_pointer::zv1::client::zwp_relative_pointer_v1::{
    self, ZwpRelativePointerV1,
};

use crate::adw::prelude::*;
use crate::{glib, gtk};

struct Handler {
    moved: Box<dyn Fn(f64, f64)>,
    unlocked: Box<dyn Fn()>,
}

impl Dispatch<WlRegistry, GlobalListContents> for Handler {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

delegate_noop!(Handler: ignore WlCompositor);
delegate_noop!(Handler: ignore WlRegion);
delegate_noop!(Handler: ignore ZwpPointerConstraintsV1);
delegate_noop!(Handler: ignore ZwpRelativePointerManagerV1);

impl Dispatch<ZwpLockedPointerV1, ()> for Handler {
    fn event(
        handler: &mut Self,
        _: &ZwpLockedPointerV1,
        event: zwp_locked_pointer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwp_locked_pointer_v1::Event::Unlocked = event {
            (handler.unlocked)();
        }
    }
}

impl Dispatch<ZwpRelativePointerV1, ()> for Handler {
    fn event(
        handler: &mut Self,
        _: &ZwpRelativePointerV1,
        event: zwp_relative_pointer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwp_relative_pointer_v1::Event::RelativeMotion { dx, dy, .. } = event {
            (handler.moved)(dx, dy);
        }
    }
}

/// The pointer locked over a widget's window, until dropped.
pub(super) struct PointerLock {
    locked: ZwpLockedPointerV1,
    relative: ZwpRelativePointerV1,
    source: Option<glib::SourceId>,
    connection: Connection,
}

impl PointerLock {
    /// Lock the pointer once it is over `widget`, and report how far it moves to `moved`,
    /// until the compositor lets go of it, which `unlocked` hears of. Nothing where the
    /// session is not Wayland's, or its compositor cannot lock the pointer.
    pub fn new(
        widget: &gtk::Widget,
        moved: impl Fn(f64, f64) + 'static,
        unlocked: impl Fn() + 'static,
    ) -> Option<Self> {
        let display = widget.display().downcast::<WaylandDisplay>().ok()?;
        let native = widget.native()?;
        let surface = native
            .surface()?
            .downcast::<WaylandSurface>()
            .ok()?
            .wl_surface()?;
        let bounds = widget.compute_bounds(&native)?;
        let (left, top) = native.surface_transform();
        let pointer = display
            .default_seat()?
            .pointer()?
            .downcast::<WaylandDevice>()
            .ok()?
            .wl_pointer()?;
        let connection = Connection::from_backend(display.wl_display()?.backend().upgrade()?);
        let (globals, mut queue) = registry_queue_init::<Handler>(&connection).ok()?;
        let handle = queue.handle();
        let constraints: ZwpPointerConstraintsV1 = globals.bind(&handle, 1..=1, ()).ok()?;
        let manager: ZwpRelativePointerManagerV1 = globals.bind(&handle, 1..=1, ()).ok()?;
        let compositor: WlCompositor = globals.bind(&handle, 1..=1, ()).ok()?;
        // Locked anywhere else in the window, the pointer would click what is there.
        let region = compositor.create_region(&handle, ());
        region.add(
            (f64::from(bounds.x()) + left) as i32,
            (f64::from(bounds.y()) + top) as i32,
            bounds.width() as i32,
            bounds.height() as i32,
        );
        let locked = constraints.lock_pointer(
            &surface,
            &pointer,
            Some(&region),
            Lifetime::Oneshot,
            &handle,
            (),
        );
        let relative = manager.get_relative_pointer(&pointer, &handle, ());
        // What they made outlives them.
        region.destroy();
        constraints.destroy();
        manager.destroy();
        let mut handler = Handler {
            moved: Box::new(moved),
            unlocked: Box::new(unlocked),
        };
        // Whichever of GDK and this reads the connection queues the other's events.
        let fd = connection.backend().poll_fd().as_raw_fd();
        let source = glib_unix::unix_fd_add_local(fd, glib::IOCondition::IN, move |_, _| {
            let _ = queue.dispatch_pending(&mut handler);
            if let Some(guard) = queue.prepare_read() {
                let _ = guard.read();
            }
            let _ = queue.dispatch_pending(&mut handler);
            glib::ControlFlow::Continue
        });
        let _ = connection.flush();
        Some(Self {
            locked,
            relative,
            source: Some(source),
            connection,
        })
    }
}

impl Drop for PointerLock {
    fn drop(&mut self) {
        self.locked.destroy();
        self.relative.destroy();
        if let Some(source) = self.source.take() {
            source.remove();
        }
        let _ = self.connection.flush();
    }
}
