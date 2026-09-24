//! What libvirt tells of its machines as it happens: they start, stop, change or go, or
//! the connection itself is lost.

use std::ffi::{CStr, c_char, c_int, c_void};
use std::sync::Arc;

use virt::sys;

use super::{Hypervisor, Result, message};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// Some machine started, stopped, was defined, changed or went away.
    Changed,
    /// The connection is gone, as libvirt shut down or the network to it failed.
    Closed,
    /// A device of the host appeared, by its node device name, such as a USB device that
    /// was plugged in.
    DeviceAdded(String),
}

type Notify = Arc<dyn Fn(Event) + Send + Sync>;

/// What `Hypervisor` keeps to stop listening again.
#[derive(Debug, Default)]
pub(super) struct Watch {
    callbacks: Vec<c_int>,
    device_callback: Option<c_int>,
    close: bool,
}

impl Hypervisor {
    /// Have `notify` called, on the main loop, whenever a machine changes and when the
    /// connection closes. It stays until the connection does.
    pub fn watch(&self, notify: impl Fn(Event) + Send + Sync + 'static) -> Result<()> {
        let notify: Notify = Arc::new(notify);
        let conn = self.conn.as_ptr();
        let mut watch = self.watch.lock().unwrap_or_else(|e| e.into_inner());
        // Each kind of event has a callback of its own shape; libvirt's C API takes them
        // all as the generic one and calls each with its own arguments.
        let events: [(
            sys::virDomainEventID,
            sys::virConnectDomainEventGenericCallback,
        ); 5] = [
            (
                sys::VIR_DOMAIN_EVENT_ID_LIFECYCLE,
                // SAFETY: libvirt calls a lifecycle callback with two ints after the domain.
                Some(unsafe { std::mem::transmute::<TwoInts, Generic>(two_ints) }),
            ),
            (
                sys::VIR_DOMAIN_EVENT_ID_DEVICE_ADDED,
                // SAFETY: as above, with the device's alias.
                Some(unsafe { std::mem::transmute::<Alias, Generic>(alias) }),
            ),
            (
                sys::VIR_DOMAIN_EVENT_ID_DEVICE_REMOVED,
                // SAFETY: as above.
                Some(unsafe { std::mem::transmute::<Alias, Generic>(alias) }),
            ),
            (
                sys::VIR_DOMAIN_EVENT_ID_TRAY_CHANGE,
                // SAFETY: as above, with the alias and why the tray moved.
                Some(unsafe { std::mem::transmute::<AliasInt, Generic>(alias_int) }),
            ),
            (
                sys::VIR_DOMAIN_EVENT_ID_METADATA_CHANGE,
                // SAFETY: as above, with the kind of metadata and its namespace.
                Some(unsafe { std::mem::transmute::<IntUri, Generic>(int_uri) }),
            ),
        ];
        for (id, callback) in events {
            let opaque = Box::into_raw(Box::new(notify.clone()));
            // SAFETY: the connection is valid; libvirt keeps `opaque` until the callback
            // is deregistered, then hands it to `free_notify`.
            let registered = unsafe {
                sys::virConnectDomainEventRegisterAny(
                    conn,
                    std::ptr::null_mut(),
                    id as c_int,
                    callback,
                    opaque.cast(),
                    Some(free_notify),
                )
            };
            if registered == -1 {
                // SAFETY: libvirt did not take `opaque`.
                drop(unsafe { Box::from_raw(opaque) });
                return Err(message(virt::error::Error::last_error()));
            }
            watch.callbacks.push(registered);
        }
        let opaque = Box::into_raw(Box::new(notify.clone()));
        // SAFETY: as for the domain events; libvirt calls a lifecycle callback with two
        // ints after the device.
        let registered = unsafe {
            sys::virConnectNodeDeviceEventRegisterAny(
                conn,
                std::ptr::null_mut(),
                sys::VIR_NODE_DEVICE_EVENT_ID_LIFECYCLE as c_int,
                Some(std::mem::transmute::<DeviceLifecycle, DeviceGeneric>(
                    device_lifecycle,
                )),
                opaque.cast(),
                Some(free_notify),
            )
        };
        // Without the node device service, nothing tells of a USB device plugged in again,
        // which then stays the host's.
        if registered == -1 {
            // SAFETY: libvirt did not take `opaque`.
            drop(unsafe { Box::from_raw(opaque) });
        } else {
            watch.device_callback = Some(registered);
        }
        let opaque = Box::into_raw(Box::new(notify));
        // SAFETY: as for the domain events.
        let registered = unsafe {
            sys::virConnectRegisterCloseCallback(
                conn,
                Some(closed),
                opaque.cast(),
                Some(free_notify),
            )
        };
        if registered == -1 {
            // SAFETY: libvirt did not take `opaque`.
            drop(unsafe { Box::from_raw(opaque) });
        } else {
            watch.close = true;
        }
        Ok(())
    }

    /// Stop the callbacks `watch` registered.
    pub(super) fn unwatch(&self) {
        let conn = self.conn.as_ptr();
        let mut watch = self.watch.lock().unwrap_or_else(|e| e.into_inner());
        for id in watch.callbacks.drain(..) {
            // SAFETY: `id` is a callback this connection registered and still has.
            unsafe { sys::virConnectDomainEventDeregisterAny(conn, id) };
        }
        if let Some(id) = watch.device_callback.take() {
            // SAFETY: as above.
            unsafe { sys::virConnectNodeDeviceEventDeregisterAny(conn, id) };
        }
        if std::mem::take(&mut watch.close) {
            // SAFETY: the close callback is registered on this connection.
            unsafe { sys::virConnectUnregisterCloseCallback(conn, Some(closed)) };
        }
    }
}

type DeviceGeneric = unsafe extern "C" fn(sys::virConnectPtr, sys::virNodeDevicePtr, *mut c_void);
type DeviceLifecycle =
    unsafe extern "C" fn(sys::virConnectPtr, sys::virNodeDevicePtr, c_int, c_int, *mut c_void);
type Generic = unsafe extern "C" fn(sys::virConnectPtr, sys::virDomainPtr, *mut c_void);
type TwoInts =
    unsafe extern "C" fn(sys::virConnectPtr, sys::virDomainPtr, c_int, c_int, *mut c_void);
type Alias =
    unsafe extern "C" fn(sys::virConnectPtr, sys::virDomainPtr, *const c_char, *mut c_void);
type AliasInt =
    unsafe extern "C" fn(sys::virConnectPtr, sys::virDomainPtr, *const c_char, c_int, *mut c_void);
type IntUri =
    unsafe extern "C" fn(sys::virConnectPtr, sys::virDomainPtr, c_int, *const c_char, *mut c_void);

/// Hand `event` to the `Notify` that `opaque` points at.
///
/// # Safety
///
/// `opaque` is one that `watch` registered and libvirt has not freed yet.
unsafe fn notify(opaque: *mut c_void, event: Event) {
    // SAFETY: the caller's.
    let notify = unsafe { &*(opaque as *const Notify) };
    notify(event);
}

unsafe extern "C" fn two_ints(
    _: sys::virConnectPtr,
    _: sys::virDomainPtr,
    _: c_int,
    _: c_int,
    opaque: *mut c_void,
) {
    // SAFETY: libvirt passes the opaque this callback was registered with.
    unsafe { notify(opaque, Event::Changed) };
}

unsafe extern "C" fn alias(
    _: sys::virConnectPtr,
    _: sys::virDomainPtr,
    _: *const c_char,
    opaque: *mut c_void,
) {
    // SAFETY: as in `two_ints`.
    unsafe { notify(opaque, Event::Changed) };
}

unsafe extern "C" fn alias_int(
    _: sys::virConnectPtr,
    _: sys::virDomainPtr,
    _: *const c_char,
    _: c_int,
    opaque: *mut c_void,
) {
    // SAFETY: as in `two_ints`.
    unsafe { notify(opaque, Event::Changed) };
}

unsafe extern "C" fn int_uri(
    _: sys::virConnectPtr,
    _: sys::virDomainPtr,
    _: c_int,
    _: *const c_char,
    opaque: *mut c_void,
) {
    // SAFETY: as in `two_ints`.
    unsafe { notify(opaque, Event::Changed) };
}

unsafe extern "C" fn device_lifecycle(
    _: sys::virConnectPtr,
    dev: sys::virNodeDevicePtr,
    event: c_int,
    _: c_int,
    opaque: *mut c_void,
) {
    if event != sys::VIR_NODE_DEVICE_EVENT_CREATED as c_int {
        return;
    }
    // SAFETY: `dev` is valid for the call, and its name lives as long as it does.
    let name = unsafe { sys::virNodeDeviceGetName(dev) };
    if name.is_null() {
        return;
    }
    // SAFETY: a NUL-terminated string libvirt owns, copied before the call returns.
    let name = unsafe { CStr::from_ptr(name) }
        .to_string_lossy()
        .into_owned();
    // SAFETY: as in `two_ints`.
    unsafe { notify(opaque, Event::DeviceAdded(name)) };
}

unsafe extern "C" fn closed(_: sys::virConnectPtr, _reason: c_int, opaque: *mut c_void) {
    // SAFETY: as in `two_ints`.
    unsafe { notify(opaque, Event::Closed) };
}

unsafe extern "C" fn free_notify(opaque: *mut c_void) {
    // SAFETY: `opaque` is a box `watch` leaked, which libvirt no longer uses.
    drop(unsafe { Box::from_raw(opaque as *mut Notify) });
}
