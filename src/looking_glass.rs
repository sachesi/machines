//! A Looking Glass client: the guest's screen and pointer, as the Looking Glass host
//! application in the guest shares them through a kvmfr device, read on a thread of its
//! own, the way Looking Glass B7 does: LGMP 6 and KVMFR 20.

mod lgmp;

use std::fs::OpenOptions;
use std::io;
use std::os::fd::AsRawFd;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use lgmp::{Client, Error, Message, Queue, Shm};

use crate::passthrough;

const KVMFR_MAGIC: &[u8; 8] = b"KVMFR---";
const KVMFR_VERSION: u32 = 20;
const QUEUE_POINTER: u32 = 1;
const QUEUE_FRAME: u32 = 2;

/// The size of `struct KVMFR`, which starts what the host tells its clients, and where
/// its version string is.
const KVMFR_SIZE: usize = 48;
const KVMFR_HOSTVER: std::ops::Range<usize> = 12..44;
const KVMFR_FEATURES: usize = 44;
/// The host can put the guest's pointer where a client asks, with `KVMFRSetCursorPos`.
const KVMFR_FEATURE_SETCURSORPOS: u32 = 0x1;
const KVMFR_MESSAGE_SETCURSORPOS: u32 = 0;
/// The size of `struct KVMFRRecord`'s header, the records after `struct KVMFR` start with.
const RECORD_HEADER: usize = 8;
const RECORD_VMINFO: u8 = 1;

/// The size of `struct KVMFRFrame`, and of `struct FrameBuffer`'s write pointer, which
/// the pixels follow.
const FRAME_HEADER: usize = 1084;
const WRITE_POINTER: usize = 4;
const MAX_DAMAGE_RECTS: usize = 64;
const FRAME_TYPE_BGRA: u32 = 1;
const FRAME_TYPE_RGBA: u32 = 2;
const FRAME_TYPE_RGBA10: u32 = 3;
const FRAME_TYPE_RGBA16F: u32 = 4;
const FRAME_TYPE_BGR_32: u32 = 5;
const FRAME_TYPE_RGB_24: u32 = 6;
const FRAME_FLAG_TRUNCATED: u32 = 0x4;
const FRAME_FLAG_HDR_PQ: u32 = 0x10;
/// The longest side of a screen taken.
const MAX_SIDE: u32 = 16384;

/// The size of `struct KVMFRCursor`, which a shape's pixels follow.
const CURSOR_HEADER: usize = 24;
const CURSOR_FLAG_POSITION: u32 = 0x1;
const CURSOR_FLAG_VISIBLE: u32 = 0x2;
const CURSOR_FLAG_SHAPE: u32 = 0x4;
const CURSOR_TYPE_COLOR: u32 = 0;
const CURSOR_TYPE_MONOCHROME: u32 = 1;
const CURSOR_TYPE_MASKED_COLOR: u32 = 2;
const MAX_CURSOR_SIDE: u32 = 512;

/// How often the host is looked for while it does not run.
const JOIN_POLL: Duration = Duration::from_millis(250);
/// How long to wait before trying again, after a host that cannot be shown.
const RETRY: Duration = Duration::from_secs(1);
/// How often the queues are looked at while they are empty.
const POLL: Duration = Duration::from_millis(1);
/// How often the host's heartbeat is checked.
const ALIVE_CHECK: Duration = Duration::from_millis(100);
/// How long a frame's pixels may stop coming before the frame is given up.
const FRAME_STALL: Duration = Duration::from_millis(500);
/// How many pixel buffers are kept to use again once GTK lets go of them.
const POOL: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// The host application does not run in the guest, or not yet.
    Waiting,
    /// Its frames come.
    Showing,
    /// It is a version this client cannot read; which, if it says.
    Incompatible(Option<String>),
    /// The device shares another machine's screen.
    OtherMachine,
    /// The device is too small for the guest's screen, which needs this many MiB.
    TooSmall(u64),
    /// This user may not open the device.
    Denied,
    /// The device cannot be read, for this reason.
    Failed(String),
}

/// How a frame's pixels are laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// B, G, R and a byte unused.
    Bgrx,
    /// R, G, B and a byte unused.
    Rgbx,
    /// B, G, R.
    Bgr,
    /// R, G, B.
    Rgb,
    /// R, G, B, 16 bits each, widened from the guest's 10.
    Rgb16,
    /// As `Rgb16`, in the PQ curve of an HDR screen.
    Rgb16Pq,
    /// R, G, B, A, half floats, in linear light.
    Rgba16Float,
}

/// A frame's pixels, in a buffer the reader uses again once nothing else holds it.
#[derive(Debug, Clone)]
pub struct Pixels(Arc<Vec<u8>>);

impl AsRef<[u8]> for Pixels {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

#[derive(Debug, Clone)]
pub struct Frame {
    pub pixels: Pixels,
    pub format: Format,
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    /// The size of the guest's screen, which its pointer moves in, and which the frame
    /// may be scaled down from.
    pub screen: (u32, u32),
    /// How many quarter turns clockwise the frame is to be shown turned, as the guest's
    /// screen is.
    pub turns: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// A pointer shape, in premultiplied B, G, R, A.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shape {
    pub pixels: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub hotspot: (i32, i32),
}

/// What changed since it was last taken.
#[derive(Debug, Default)]
pub struct Update {
    pub status: Option<Status>,
    pub frame: Option<Frame>,
    /// What changed from the frame before the one in `frame`, `None` for all of it.
    pub damage: Option<Vec<Rect>>,
    pub shape: Option<Shape>,
    pub visible: Option<bool>,
    /// Where the guest's pointer is, on its screen.
    pub position: Option<(i32, i32)>,
    /// Whether the host puts the guest's pointer where [`Reader::place`] says.
    pub places_pointer: Option<bool>,
}

#[derive(Default)]
struct Shared {
    update: Update,
    /// Whether the other side was told of the update, and has not taken it yet.
    woken: bool,
    /// Where to put the guest's pointer, not yet asked of the host.
    place: Option<(i32, i32)>,
}

/// Reads a kvmfr device on a thread of its own, until dropped.
pub struct Reader {
    shared: Arc<Mutex<Shared>>,
    stop: Arc<AtomicBool>,
}

impl Reader {
    /// Read the kvmfr device at `path` for the machine of UUID `uuid`. The thread calls
    /// `wake` when there is something to take, once until it is taken.
    pub fn start(path: &str, uuid: &str, wake: impl Fn() + Send + 'static) -> Self {
        let shared: Arc<Mutex<Shared>> = Arc::default();
        let stop: Arc<AtomicBool> = Arc::default();
        let mut worker = Worker {
            shared: shared.clone(),
            stop: stop.clone(),
            wake: Box::new(wake),
            uuid: parse_uuid(uuid),
            status: None,
            pool: Vec::new(),
            raw: Vec::new(),
            serial: None,
            whole: true,
        };
        let path = path.to_owned();
        if let Err(e) = thread::Builder::new()
            .name("looking-glass".to_owned())
            .spawn(move || worker.run(&path))
        {
            lock(&shared).update.status = Some(Status::Failed(e.to_string()));
        }
        Self { shared, stop }
    }

    /// Have the host put the guest's pointer at (`x`, `y`) on its screen, if it can.
    pub fn place(&self, x: i32, y: i32) {
        lock(&self.shared).place = Some((x, y));
    }

    pub fn take(&self) -> Update {
        let mut shared = lock(&self.shared);
        shared.woken = false;
        std::mem::take(&mut shared.update)
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn lock(shared: &Mutex<Shared>) -> std::sync::MutexGuard<'_, Shared> {
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A UUID as libvirt writes it, in the order of its bytes as the host sends it.
fn parse_uuid(uuid: &str) -> Option<[u8; 16]> {
    let hex: Vec<u8> = uuid.bytes().filter(|&b| b != b'-').collect();
    let mut bytes = [0; 16];
    for (byte, pair) in bytes.iter_mut().zip(hex.chunks_exact(2)) {
        *byte = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
    }
    (hex.len() == 32).then_some(bytes)
}

/// A kvmfr device, mapped until dropped.
struct Mapping {
    ptr: NonNull<u8>,
    len: usize,
}

impl Mapping {
    fn open(path: &str) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let len = match passthrough::kvmfr_size(&file) {
            Ok(len) => len,
            // A plain file, as QEMU can share one in place of the device.
            Err(e) if e.raw_os_error() == Some(libc::ENOTTY) => file.metadata()?.len(),
            Err(e) => return Err(e),
        };
        let len = usize::try_from(len)
            .ok()
            .filter(|&len| len > 0)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "it has no memory"))?;
        // SAFETY: a new shared mapping of the whole file, which is `len` bytes long.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let ptr = NonNull::new(ptr.cast()).ok_or_else(|| io::Error::from(io::ErrorKind::Other))?;
        Ok(Self { ptr, len })
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: the mapping made in `open`, which nothing uses any more.
        unsafe { libc::munmap(self.ptr.as_ptr().cast(), self.len) };
    }
}

/// What the host tells its clients as they join.
#[derive(Debug, PartialEq, Eq)]
struct Welcome {
    /// The guest's UUID, when it has one.
    uuid: Option<[u8; 16]>,
    places_pointer: bool,
}

impl Welcome {
    /// What `udata` says, or, for a host of another version, which one it says it is.
    fn parse(udata: &[u8]) -> Result<Self, Option<String>> {
        if udata.len() < KVMFR_SIZE || &udata[..8] != KVMFR_MAGIC {
            return Err(None);
        }
        if u32_at(udata, 8) != KVMFR_VERSION {
            let hostver = &udata[KVMFR_HOSTVER];
            let end = hostver
                .iter()
                .position(|&b| b == 0)
                .unwrap_or(hostver.len());
            let hostver = String::from_utf8_lossy(&hostver[..end]).into_owned();
            return Err(Some(hostver).filter(|v| !v.is_empty()));
        }
        let mut uuid = None;
        let mut records = &udata[KVMFR_SIZE..];
        while records.len() >= RECORD_HEADER {
            let size = u32_at(records, 4) as usize;
            let Some(data) = records[RECORD_HEADER..].get(..size) else {
                break;
            };
            if records[0] == RECORD_VMINFO && data.len() >= 16 && data[..16].iter().any(|&b| b != 0)
            {
                uuid = data[..16].try_into().ok();
            }
            records = &records[RECORD_HEADER + size..];
        }
        Ok(Self {
            uuid,
            places_pointer: u32_at(udata, KVMFR_FEATURES) & KVMFR_FEATURE_SETCURSORPOS != 0,
        })
    }
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("4 bytes"))
}

/// A frame's header, as `struct KVMFRFrame` has it.
#[derive(Debug, PartialEq, Eq)]
struct FrameHeader {
    serial: u32,
    kind: u32,
    screen: (u32, u32),
    width: u32,
    height: u32,
    pitch: u32,
    turns: u32,
    /// Where the pixels' write pointer is, from the header.
    offset: u32,
    damage: Option<Vec<Rect>>,
    flags: u32,
}

impl FrameHeader {
    fn parse(h: &[u8; FRAME_HEADER]) -> Self {
        let count = u32_at(h, 52) as usize;
        let damage = (1..=MAX_DAMAGE_RECTS).contains(&count).then(|| {
            (0..count)
                .map(|i| {
                    let at = 56 + 16 * i;
                    Rect {
                        x: u32_at(h, at),
                        y: u32_at(h, at + 4),
                        width: u32_at(h, at + 8),
                        height: u32_at(h, at + 12),
                    }
                })
                .collect()
        });
        Self {
            serial: u32_at(h, 4),
            kind: u32_at(h, 8),
            screen: (u32_at(h, 12), u32_at(h, 16)),
            width: u32_at(h, 28),
            height: u32_at(h, 32),
            pitch: u32_at(h, 44),
            // FRAME_ROT_0 to FRAME_ROT_270; Looking Glass takes anything else as 0.
            turns: Some(u32_at(h, 36)).filter(|&r| r < 4).unwrap_or(0),
            offset: u32_at(h, 48),
            damage,
            flags: u32_at(h, 1080),
        }
    }

    /// How the pixels are laid out, as they come and as they are handed on, and how many
    /// bytes a pixel takes as they come.
    fn format(&self) -> Option<(Format, usize)> {
        Some(match self.kind {
            FRAME_TYPE_BGRA => (Format::Bgrx, 4),
            FRAME_TYPE_RGBA => (Format::Rgbx, 4),
            FRAME_TYPE_RGBA10 if self.flags & FRAME_FLAG_HDR_PQ != 0 => (Format::Rgb16Pq, 4),
            FRAME_TYPE_RGBA10 => (Format::Rgb16, 4),
            FRAME_TYPE_RGBA16F => (Format::Rgba16Float, 8),
            // Packed in the rows of a texture of 32-bit pixels, a quarter narrower.
            FRAME_TYPE_BGR_32 => (Format::Bgr, 3),
            FRAME_TYPE_RGB_24 => (Format::Rgb, 3),
            _ => return None,
        })
    }
}

/// R, G, B of 10 bits in 32, widened to 16 bits each.
fn widen_rgb10(src: &[u8], width: usize, height: usize, pitch: usize, dst: &mut Vec<u8>) {
    dst.clear();
    dst.reserve(width * height * 6);
    for row in src.chunks(pitch).take(height) {
        for pixel in row[..width * 4].chunks_exact(4) {
            let v = u32::from_le_bytes(pixel.try_into().expect("4 bytes"));
            for channel in [v & 0x3ff, v >> 10 & 0x3ff, v >> 20 & 0x3ff] {
                let wide = (channel << 6 | channel >> 4) as u16;
                dst.extend_from_slice(&wide.to_ne_bytes());
            }
        }
    }
}

/// A pointer shape of `kind` as premultiplied B, G, R, A, and its height, which a
/// monochrome shape has half of what it says, its two masks one above the other.
///
/// GTK cannot invert what is under the pointer, which monochrome and masked shapes can,
/// so those pixels are black, or the masked shape's color.
fn cursor_pixels(
    kind: u32,
    width: usize,
    height: usize,
    pitch: usize,
    data: &[u8],
) -> Option<(Vec<u8>, usize)> {
    if pitch == 0 {
        return None;
    }
    let mut pixels = Vec::with_capacity(width * height * 4);
    let rows = |height: usize| data.chunks(pitch).take(height);
    match kind {
        CURSOR_TYPE_COLOR => {
            for row in rows(height) {
                pixels.extend_from_slice(row.get(..width * 4)?);
            }
            Some((pixels, height))
        }
        CURSOR_TYPE_MASKED_COLOR => {
            for row in rows(height) {
                for pixel in row.get(..width * 4)?.chunks_exact(4) {
                    let masked = pixel[3] != 0;
                    let color = [pixel[0], pixel[1], pixel[2]];
                    if masked && color == [0; 3] {
                        pixels.extend_from_slice(&[0; 4]);
                    } else {
                        pixels.extend_from_slice(&[color[0], color[1], color[2], 255]);
                    }
                }
            }
            Some((pixels, height))
        }
        CURSOR_TYPE_MONOCHROME => {
            let height = height / 2;
            if pitch < width.div_ceil(8) {
                return None;
            }
            let masks = data.get(..pitch * height * 2)?;
            let (and, xor) = masks.split_at(pitch * height);
            for y in 0..height {
                for x in 0..width {
                    let bit = |mask: &[u8]| mask[y * pitch + x / 8] & (0x80 >> (x % 8)) != 0;
                    pixels.extend_from_slice(match (bit(and), bit(xor)) {
                        (true, false) => &[0, 0, 0, 0],
                        (false, true) => &[255, 255, 255, 255],
                        _ => &[0, 0, 0, 255],
                    });
                }
            }
            Some((pixels, height))
        }
        _ => None,
    }
}

/// What the host needs of the device for a screen `height` rows of `pitch` bytes: two
/// frames and ten MiB, to the next power of two, as Looking Glass reckons it.
fn needed_mib(height: u32, pitch: u32) -> u64 {
    let needed = (u64::from(height) * u64::from(pitch) * 2).div_ceil(1 << 20) + 10;
    needed.next_power_of_two()
}

struct Worker {
    shared: Arc<Mutex<Shared>>,
    stop: Arc<AtomicBool>,
    wake: Box<dyn Fn() + Send>,
    uuid: Option<[u8; 16]>,
    status: Option<Status>,
    pool: Vec<Arc<Vec<u8>>>,
    /// The pixels as they came, for those handed on in another format.
    raw: Vec<u8>,
    /// The last frame's number, to know it when the host sends it again.
    serial: Option<u32>,
    /// Whether the next frame handed on has to be taken whole, as the one before it was
    /// not handed on.
    whole: bool,
}

impl Worker {
    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    /// Sleep for `time`, or less if asked to stop; whether asked to.
    fn pause(&self, time: Duration) -> bool {
        let until = Instant::now() + time;
        while !self.stopped() {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            thread::sleep(left.min(Duration::from_millis(50)));
        }
        true
    }

    fn publish(&self, change: impl FnOnce(&mut Update)) {
        let mut shared = lock(&self.shared);
        change(&mut shared.update);
        if !std::mem::replace(&mut shared.woken, true) {
            drop(shared);
            (self.wake)();
        }
    }

    fn set_status(&mut self, status: Status) {
        if self.status.as_ref() != Some(&status) {
            self.status = Some(status.clone());
            self.publish(|update| update.status = Some(status));
        }
    }

    fn run(&mut self, path: &str) {
        let mapping = match Mapping::open(path) {
            Ok(mapping) => mapping,
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                self.set_status(Status::Denied);
                return;
            }
            Err(e) => {
                self.set_status(Status::Failed(format!("{path}: {e}")));
                return;
            }
        };
        // SAFETY: mmap aligns to a page, and the mapping outlives `shm`.
        let shm = unsafe { Shm::new(mapping.ptr, mapping.len) };
        self.set_status(Status::Waiting);
        let id = (SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos())
            ^ std::process::id())
            | 1;
        while !self.stopped() {
            let status = self.session(&shm, id);
            let pause = if status == Status::Waiting {
                JOIN_POLL
            } else {
                RETRY
            };
            self.set_status(status);
            self.pause(pause);
        }
    }

    /// Join the host's session and follow it until it ends; what to show then.
    fn session(&mut self, shm: &Shm, id: u32) -> Status {
        let Ok(mut client) = Client::new(shm, id) else {
            return Status::Waiting;
        };
        let udata = loop {
            // The heartbeat has to move for the host to count as running.
            if self.pause(JOIN_POLL) {
                return Status::Waiting;
            }
            match client.join() {
                Ok(udata) => break udata,
                Err(Error::Version) => return Status::Incompatible(None),
                Err(_) => {}
            }
        };
        let welcome = match Welcome::parse(&udata) {
            Ok(welcome) => welcome,
            Err(hostver) => return Status::Incompatible(hostver),
        };
        if let (Some(want), Some(have)) = (self.uuid, welcome.uuid)
            && want != have
        {
            return Status::OtherMachine;
        }
        let Some(mut frames) = self.subscribe(&client, QUEUE_FRAME) else {
            return Status::Waiting;
        };
        let Some(mut pointer) = self.subscribe(&client, QUEUE_POINTER) else {
            frames.unsubscribe(&client);
            return Status::Waiting;
        };
        self.serial = None;
        self.whole = true;
        lock(&self.shared).place = None;
        self.publish(|update| update.places_pointer = Some(welcome.places_pointer));
        self.follow(shm, &mut client, &mut frames, &mut pointer);
        frames.unsubscribe(&client);
        pointer.unsubscribe(&client);
        Status::Waiting
    }

    /// Subscribe to the queue `queue_id`, which a host just started may not have made yet.
    fn subscribe(&self, client: &Client, queue_id: u32) -> Option<Queue> {
        let started = Instant::now();
        loop {
            match client.subscribe(queue_id) {
                Ok(queue) => return Some(queue),
                Err(Error::NoSuchQueue) if started.elapsed() < RETRY && !self.pause(POLL) => {}
                Err(_) => return None,
            }
        }
    }

    /// Hand on the frames and pointer the host posts, until its session ends.
    fn follow(&mut self, shm: &Shm, client: &mut Client, frames: &mut Queue, pointer: &mut Queue) {
        let mut checked = Instant::now();
        let mut placing = None;
        while !self.stopped() {
            if self.place_pointer(client, pointer, &mut placing).is_err() {
                return;
            }
            let mut busy = false;
            match frames.peek(client) {
                Ok(message) => {
                    busy = true;
                    let read = self.frame(shm, &message);
                    if read.is_err() || frames.done(client).is_err() {
                        return;
                    }
                }
                Err(Error::Empty) => {}
                Err(_) => return,
            }
            match pointer.peek(client) {
                Ok(message) => {
                    busy = true;
                    let read = self.pointer(shm, &message);
                    if read.is_err() || pointer.done(client).is_err() {
                        return;
                    }
                }
                Err(Error::Empty) => {}
                Err(_) => return,
            }
            if checked.elapsed() >= ALIVE_CHECK {
                if !client.alive() {
                    return;
                }
                checked = Instant::now();
            }
            if !busy {
                thread::sleep(POLL);
            }
        }
    }

    /// Ask the host to put the guest's pointer where it was last wanted, once the host has
    /// taken the last such request, `placing`.
    fn place_pointer(
        &self,
        client: &Client,
        pointer: &Queue,
        placing: &mut Option<u32>,
    ) -> Result<(), Error> {
        if let Some(sent) = *placing {
            if (pointer.received(client)?.wrapping_sub(sent) as i32) < 0 {
                return Ok(());
            }
            *placing = None;
        }
        let Some((x, y)) = lock(&self.shared).place.take() else {
            return Ok(());
        };
        let mut message = [0; 12];
        message[..4].copy_from_slice(&KVMFR_MESSAGE_SETCURSORPOS.to_le_bytes());
        message[4..8].copy_from_slice(&x.to_le_bytes());
        message[8..].copy_from_slice(&y.to_le_bytes());
        match pointer.send(client, &message) {
            Ok(sent) => *placing = Some(sent),
            Err(Error::Full) => {
                lock(&self.shared).place.get_or_insert((x, y));
            }
            Err(e) => return Err(e),
        }
        Ok(())
    }

    /// A buffer of `len` bytes nothing else holds, from the pool if one there is free.
    fn buffer(&mut self, len: usize) -> Arc<Vec<u8>> {
        let free = self.pool.iter_mut().position(|b| Arc::get_mut(b).is_some());
        let mut buffer = match free {
            Some(i) => self.pool.swap_remove(i),
            None => Arc::default(),
        };
        let bytes = Arc::get_mut(&mut buffer).expect("a buffer nothing else holds");
        bytes.clear();
        bytes.resize(len, 0);
        buffer
    }

    /// Copy the pixels at `data`, which the host may still be writing, as far as the
    /// write pointer at `write_pointer` says it has; whether they all came in time.
    fn copy_pixels(
        shm: &Shm,
        write_pointer: usize,
        data: usize,
        dst: &mut [u8],
    ) -> Result<bool, Error> {
        let written = shm.atomic_u32(write_pointer)?;
        let mut done = 0;
        let mut progressed = Instant::now();
        while done < dst.len() {
            let now = (written.load(Ordering::Acquire) as usize).min(dst.len());
            if now > done {
                shm.read(data + done, &mut dst[done..now])?;
                done = now;
                progressed = Instant::now();
            } else if progressed.elapsed() > FRAME_STALL {
                return Ok(false);
            } else {
                thread::sleep(Duration::from_micros(20));
            }
        }
        Ok(true)
    }

    fn frame(&mut self, shm: &Shm, message: &Message) -> Result<(), Error> {
        let mut header = [0; FRAME_HEADER];
        if message.size < FRAME_HEADER {
            return Err(Error::Corrupted);
        }
        shm.read(message.offset, &mut header)?;
        let header = FrameHeader::parse(&header);
        // A host sends its last frame again as a client subscribes.
        if self.serial.replace(header.serial) == Some(header.serial) {
            return Ok(());
        }
        let Some((format, bpp)) = header.format() else {
            self.whole = true;
            return Ok(());
        };
        let (width, height, pitch) = (header.width, header.height, header.pitch);
        if !(1..=MAX_SIDE).contains(&width)
            || !(1..=MAX_SIDE).contains(&height)
            || (pitch as usize) < width as usize * bpp
        {
            return Err(Error::Corrupted);
        }
        let size = pitch as usize * height as usize;
        let fits = (header.offset as usize)
            .checked_add(WRITE_POINTER + size)
            .is_some_and(|end| end <= message.size);
        if !fits || header.flags & FRAME_FLAG_TRUNCATED != 0 {
            self.whole = true;
            self.set_status(Status::TooSmall(needed_mib(
                header.screen.1.max(height),
                pitch,
            )));
            return Ok(());
        }
        let write_pointer = message.offset + header.offset as usize;
        let data = write_pointer + WRITE_POINTER;
        let (pixels, stride) = if matches!(format, Format::Rgb16 | Format::Rgb16Pq) {
            let mut raw = std::mem::take(&mut self.raw);
            raw.resize(size, 0);
            let complete = Self::copy_pixels(shm, write_pointer, data, &mut raw)?;
            let mut buffer = self.buffer(0);
            let bytes = Arc::get_mut(&mut buffer).expect("a buffer nothing else holds");
            widen_rgb10(&raw, width as usize, height as usize, pitch as usize, bytes);
            self.raw = raw;
            (complete.then_some(buffer), width as usize * 6)
        } else {
            let mut buffer = self.buffer(size);
            let bytes = Arc::get_mut(&mut buffer).expect("a buffer nothing else holds");
            let complete = Self::copy_pixels(shm, write_pointer, data, bytes)?;
            (complete.then_some(buffer), pitch as usize)
        };
        let Some(pixels) = pixels else {
            self.whole = true;
            return Ok(());
        };
        if self.pool.len() < POOL {
            self.pool.push(pixels.clone());
        }
        let frame = Frame {
            pixels: Pixels(pixels),
            format,
            width,
            height,
            stride,
            screen: header.screen,
            turns: header.turns,
        };
        let damage = header.damage.filter(|_| !std::mem::take(&mut self.whole));
        self.set_status(Status::Showing);
        self.publish(|update| {
            update.damage = match (update.frame.is_some(), update.damage.take(), damage) {
                (false, _, damage) => damage,
                (true, Some(mut before), Some(damage)) => {
                    before.extend(damage);
                    Some(before)
                }
                _ => None,
            };
            update.frame = Some(frame);
        });
        Ok(())
    }

    fn pointer(&mut self, shm: &Shm, message: &Message) -> Result<(), Error> {
        let mut header = [0; CURSOR_HEADER];
        if message.size < CURSOR_HEADER {
            return Err(Error::Corrupted);
        }
        shm.read(message.offset, &mut header)?;
        let flags = message.udata;
        let x = i16::from_le_bytes([header[0], header[1]]);
        let y = i16::from_le_bytes([header[2], header[3]]);
        let shape = if flags & CURSOR_FLAG_SHAPE != 0 {
            let kind = u32_at(&header, 4);
            let hotspot = (i32::from(header[8] as i8), i32::from(header[9] as i8));
            let (width, height, pitch) = (
                u32_at(&header, 12) as usize,
                u32_at(&header, 16) as usize,
                u32_at(&header, 20) as usize,
            );
            let side = 1..=MAX_CURSOR_SIDE as usize;
            let size = pitch * height;
            if !side.contains(&width)
                || !side.contains(&height)
                || pitch > MAX_CURSOR_SIDE as usize * 4
                || CURSOR_HEADER + size > message.size
            {
                return Err(Error::Corrupted);
            }
            let mut data = vec![0; size];
            shm.read(message.offset + CURSOR_HEADER, &mut data)?;
            cursor_pixels(kind, width, height, pitch, &data).map(|(pixels, height)| Shape {
                pixels,
                width: width as u32,
                height: height as u32,
                hotspot: (hotspot.0.max(0), hotspot.1.max(0)),
            })
        } else {
            None
        };
        self.publish(|update| {
            if shape.is_some() {
                update.shape = shape;
            }
            update.visible = Some(flags & CURSOR_FLAG_VISIBLE != 0);
            if flags & CURSOR_FLAG_POSITION != 0 {
                update.position = Some((i32::from(x), i32::from(y)));
            }
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn welcome(version: u32, records: &[(u8, &[u8])]) -> Vec<u8> {
        let mut udata = Vec::from(&KVMFR_MAGIC[..]);
        udata.extend_from_slice(&version.to_le_bytes());
        let mut hostver = [0; 32];
        hostver[..5].copy_from_slice(b"B6-12");
        udata.extend_from_slice(&hostver);
        udata.extend_from_slice(&KVMFR_FEATURE_SETCURSORPOS.to_le_bytes());
        for (kind, data) in records {
            udata.extend_from_slice(&[*kind, 0, 0, 0]);
            udata.extend_from_slice(&(data.len() as u32).to_le_bytes());
            udata.extend_from_slice(data);
        }
        udata
    }

    #[test]
    fn the_host_says_which_guest_it_is() {
        let uuid = parse_uuid("00112233-4455-6677-8899-aabbccddeeff").unwrap();
        assert_eq!(uuid[0], 0x00);
        assert_eq!(uuid[15], 0xff);
        let mut vminfo = uuid.to_vec();
        vminfo.extend_from_slice(&[0; 35]);
        let udata = welcome(
            KVMFR_VERSION,
            &[(2, b"\x03Windows"), (RECORD_VMINFO, &vminfo)],
        );
        let parsed = Welcome::parse(&udata).unwrap();
        assert_eq!(parsed.uuid, Some(uuid));
        assert!(parsed.places_pointer);
        let unknown = welcome(KVMFR_VERSION, &[(RECORD_VMINFO, &[0; 51])]);
        assert_eq!(Welcome::parse(&unknown).unwrap().uuid, None);
        assert_eq!(parse_uuid("not-a-uuid"), None);
    }

    #[test]
    fn another_version_says_which() {
        assert_eq!(
            Welcome::parse(&welcome(19, &[])),
            Err(Some("B6-12".to_owned()))
        );
        assert_eq!(Welcome::parse(b"KVMFR--"), Err(None));
    }

    #[test]
    fn records_longer_than_what_is_there_end_the_list() {
        let mut udata = welcome(KVMFR_VERSION, &[]);
        udata.extend_from_slice(&[RECORD_VMINFO, 0, 0, 0, 0xff, 0xff, 0xff, 0xff, 1, 2]);
        assert_eq!(Welcome::parse(&udata).unwrap().uuid, None);
    }

    #[test]
    fn pointer_shapes_become_premultiplied_bgra() {
        // A 2×1 shape of the AND mask above the XOR mask: black, then white.
        let mono = [0b0000_0000, 0b0100_0000];
        let (pixels, height) = cursor_pixels(CURSOR_TYPE_MONOCHROME, 2, 2, 1, &mono).unwrap();
        assert_eq!(height, 1);
        assert_eq!(pixels, [0, 0, 0, 255, 255, 255, 255, 255]);
        let (clear, _) = cursor_pixels(CURSOR_TYPE_MONOCHROME, 1, 2, 1, &[0x80, 0]).unwrap();
        assert_eq!(clear, [0, 0, 0, 0]);

        let masked = [1, 2, 3, 0, 0, 0, 0, 255, 9, 9, 9, 255];
        let (pixels, _) = cursor_pixels(CURSOR_TYPE_MASKED_COLOR, 3, 1, 12, &masked).unwrap();
        assert_eq!(pixels, [1, 2, 3, 255, 0, 0, 0, 0, 9, 9, 9, 255]);

        assert_eq!(
            cursor_pixels(CURSOR_TYPE_COLOR, 2, 1, 4, &[0; 8]),
            None,
            "short rows"
        );
        assert_eq!(
            cursor_pixels(CURSOR_TYPE_MONOCHROME, 16, 2, 1, &[0; 2]),
            None
        );
        assert_eq!(cursor_pixels(CURSOR_TYPE_COLOR, 1, 1, 0, &[]), None);
    }

    #[test]
    fn ten_bit_color_widens_to_sixteen() {
        let white_red = (0x3ffu32 | 0x3ff << 10 | 0x3ff << 20).to_le_bytes();
        let red = 0x3ffu32.to_le_bytes();
        let src = [white_red, red, [0xee; 4]].concat();
        let mut dst = Vec::new();
        widen_rgb10(&src, 2, 1, 12, &mut dst);
        let channels: Vec<u16> = dst
            .chunks_exact(2)
            .map(|c| u16::from_ne_bytes([c[0], c[1]]))
            .collect();
        assert_eq!(channels, [0xffff, 0xffff, 0xffff, 0xffff, 0, 0]);
    }

    #[test]
    fn a_device_too_small_says_what_would_do() {
        // 1080p in 32 bits: two frames and 10 MiB, 25.8 MiB, to 32.
        assert_eq!(needed_mib(1080, 1920 * 4), 32);
        assert_eq!(needed_mib(2160, 3840 * 4), 128);
    }

    fn worker() -> (Worker, Arc<Mutex<Shared>>) {
        let shared: Arc<Mutex<Shared>> = Arc::default();
        let worker = Worker {
            shared: shared.clone(),
            stop: Arc::default(),
            wake: Box::new(|| {}),
            uuid: None,
            status: None,
            pool: Vec::new(),
            raw: Vec::new(),
            serial: None,
            whole: true,
        };
        (worker, shared)
    }

    /// A 2×2 frame of `kind`, numbered `serial`, at 4096, with its pixels at 8192 and
    /// `damage` rectangles.
    fn post_frame(memory: &mut [u8], kind: u32, serial: u32, pixels: &[u8], damage: u32) {
        let frame = &mut memory[4096..4096 + FRAME_HEADER];
        let mut set =
            |at: usize, value: u32| frame[at..at + 4].copy_from_slice(&value.to_le_bytes());
        set(4, serial);
        set(8, kind);
        for at in [12, 16, 20, 24, 28, 32] {
            set(at, 2);
        }
        set(44, pixels.len() as u32 / 2);
        set(48, 4092);
        set(52, damage);
        for i in 0..damage as usize {
            set(56 + 16 * i, 1);
            set(64 + 16 * i, 1);
            set(68 + 16 * i, 1);
        }
        memory[8188..8192].copy_from_slice(&(pixels.len() as u32).to_le_bytes());
        memory[8192..8192 + pixels.len()].copy_from_slice(pixels);
    }

    #[test]
    fn frames_are_handed_on_with_what_changed() {
        let mut words = vec![0u64; 2048];
        // SAFETY: u64 has no padding, and any bytes are valid u8.
        let memory: &mut [u8] =
            unsafe { std::slice::from_raw_parts_mut(words.as_mut_ptr().cast(), 16384) };
        let pixels: Vec<u8> = (0..16).collect();
        post_frame(memory, FRAME_TYPE_BGRA, 1, &pixels, 1);
        let ptr = NonNull::new(words.as_mut_ptr().cast()).unwrap();
        // SAFETY: `words` outlives `shm`, and its u64 align it.
        let shm = unsafe { Shm::new(ptr, 16384) };
        let message = Message {
            udata: 0,
            offset: 4096,
            size: 8192,
        };
        let (mut worker, shared) = worker();

        worker.frame(&shm, &message).unwrap();
        let update = std::mem::take(&mut lock(&shared).update);
        assert_eq!(update.status, Some(Status::Showing));
        assert_eq!(update.damage, None, "the first frame is taken whole");
        let frame = update.frame.unwrap();
        assert_eq!(frame.format, Format::Bgrx);
        assert_eq!((frame.width, frame.height, frame.stride), (2, 2, 8));
        assert_eq!(frame.pixels.as_ref(), &pixels[..]);

        worker.frame(&shm, &message).unwrap();
        assert!(lock(&shared).update.frame.is_none(), "the same frame again");

        // SAFETY: as above; `shm` only reads while this does not write.
        let memory: &mut [u8] =
            unsafe { std::slice::from_raw_parts_mut(words.as_mut_ptr().cast(), 16384) };
        post_frame(memory, FRAME_TYPE_BGR_32, 2, &pixels[..12], 1);
        memory[4096 + 36] = 1;
        worker.frame(&shm, &message).unwrap();
        let update = std::mem::take(&mut lock(&shared).update);
        let rect = Rect {
            x: 1,
            y: 0,
            width: 1,
            height: 1,
        };
        assert_eq!(update.damage, Some(vec![rect]));
        let frame = update.frame.unwrap();
        assert_eq!((frame.format, frame.stride), (Format::Bgr, 6));
        assert_eq!(frame.turns, 1, "FRAME_ROT_90");
    }

    #[test]
    fn a_frame_larger_than_its_memory_asks_for_a_larger_device() {
        let mut words = vec![0u64; 2048];
        // SAFETY: as in `frames_are_handed_on_with_what_changed`.
        let memory: &mut [u8] =
            unsafe { std::slice::from_raw_parts_mut(words.as_mut_ptr().cast(), 16384) };
        post_frame(memory, FRAME_TYPE_BGRA, 1, &[0; 16], 0);
        let ptr = NonNull::new(words.as_mut_ptr().cast()).unwrap();
        // SAFETY: as there.
        let shm = unsafe { Shm::new(ptr, 16384) };
        let message = Message {
            udata: 0,
            offset: 4096,
            size: 4100,
        };
        let (mut worker, shared) = worker();
        worker.frame(&shm, &message).unwrap();
        let update = std::mem::take(&mut lock(&shared).update);
        assert_eq!(update.status, Some(Status::TooSmall(16)));
        assert!(update.frame.is_none());
    }
}
