//! A machine's serial console, as a libvirt stream that libvirt's event loop reads.

use std::ffi::{c_char, c_int, c_void};

use virt::domain::Domain;
use virt::stream::Stream;
use virt::sys;

use super::{Hypervisor, Result, message};

/// What the console hands over: bytes the guest wrote, or `None` once the console closed.
type Sink = Box<dyn Fn(Option<Vec<u8>>) + Send>;

/// Have libvirt watch its streams, those of serial consoles, from the GLib main context,
/// which the application's main loop runs. It has to be in place before the first
/// connection opens.
pub fn start_event_loop() -> Result<()> {
    virt::event::event_register_default_impl().map_err(message)
}

/// An open serial console; dropping it closes it.
pub struct SerialStream {
    stream: Stream,
}

impl Hypervisor {
    /// Open the first serial console of the machine `uuid`, taking it over from any other
    /// client. `sink` gets what the guest writes, on the main loop.
    pub fn open_serial(
        &self,
        uuid: &str,
        sink: impl Fn(Option<Vec<u8>>) + Send + 'static,
    ) -> Result<SerialStream> {
        let dom: Domain = self.domain(uuid)?;
        let stream = Stream::new(&self.conn, sys::VIR_STREAM_NONBLOCK).map_err(message)?;
        dom.open_console(None, &stream, sys::VIR_DOMAIN_CONSOLE_FORCE)
            .map_err(message)?;
        let sink: *mut Sink = Box::into_raw(Box::new(Box::new(sink)));
        let events = sys::VIR_STREAM_EVENT_READABLE
            | sys::VIR_STREAM_EVENT_ERROR
            | sys::VIR_STREAM_EVENT_HANGUP;
        // SAFETY: the stream is valid; libvirt keeps `sink` until the callback is removed,
        // then hands it to `free_sink`, which takes back the box made above.
        let added = unsafe {
            sys::virStreamEventAddCallback(
                stream.as_ptr(),
                events as c_int,
                Some(readable),
                sink.cast(),
                Some(free_sink),
            )
        };
        if added == -1 {
            // SAFETY: libvirt did not take `sink`, so it is still only this function's.
            drop(unsafe { Box::from_raw(sink) });
            return Err(message(virt::error::Error::last_error()));
        }
        Ok(SerialStream { stream })
    }
}

impl SerialStream {
    /// Type `bytes` into the console. What the stream cannot take at once is dropped
    /// rather than waited for; it is keystrokes.
    pub fn send(&self, bytes: &[u8]) {
        // SAFETY: the stream is valid while `self` is, and `bytes` outlives the call.
        unsafe {
            sys::virStreamSend(
                self.stream.as_ptr(),
                bytes.as_ptr() as *const c_char,
                bytes.len(),
            );
        }
    }
}

impl Drop for SerialStream {
    fn drop(&mut self) {
        // SAFETY: the stream is valid; removing its callback makes libvirt free the sink.
        unsafe {
            sys::virStreamEventRemoveCallback(self.stream.as_ptr());
            sys::virStreamAbort(self.stream.as_ptr());
        }
    }
}

unsafe extern "C" fn readable(stream: sys::virStreamPtr, _events: c_int, opaque: *mut c_void) {
    // SAFETY: `opaque` is the sink `open_serial` registered, alive until `free_sink`.
    let sink = unsafe { &*(opaque as *const Sink) };
    let mut buf = [0u8; 4096];
    loop {
        // SAFETY: `stream` is the one this callback is registered on, and `buf` is ours.
        let n = unsafe { sys::virStreamRecv(stream, buf.as_mut_ptr() as *mut c_char, buf.len()) };
        match n {
            // Nothing more for now.
            -2 => return,
            n if n > 0 => sink(Some(buf[..n as usize].to_vec())),
            // The end of the stream, or its failure: the console is gone either way.
            _ => {
                sink(None);
                // SAFETY: as above; no more events are wanted from a closed stream.
                unsafe { sys::virStreamEventRemoveCallback(stream) };
                return;
            }
        }
    }
}

unsafe extern "C" fn free_sink(opaque: *mut c_void) {
    // SAFETY: `opaque` is the box `open_serial` leaked, which libvirt no longer uses.
    drop(unsafe { Box::from_raw(opaque as *mut Sink) });
}
