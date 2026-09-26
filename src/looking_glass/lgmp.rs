//! The client side of LGMP, the protocol the Looking Glass host application posts its
//! messages through, in the memory the guest shares with this computer, laid out as
//! LGMP 6 lays it out.
//!
//! The guest writes that memory as it likes, so every offset and count read from it is
//! checked against the memory before it is used, and read once.

use std::ptr::NonNull;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::SeqCst};
use std::time::{Duration, Instant};

const MAGIC: u32 = 0x504d_474c;
const VERSION: u32 = 6;
const MAX_QUEUES: u32 = 5;
/// How many messages from clients a queue holds, and how large each may be.
const CLIENT_MESSAGES: u32 = 10;
const CLIENT_MESSAGE_SIZE: usize = 64;
/// How long the host may leave its heartbeat as it is before its session counts as over.
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(1);
/// How long a queue's lock is waited for, as a guest that stopped could hold it forever.
const LOCK_TIMEOUT: Duration = Duration::from_millis(100);

/// Where `struct LGMPHeader` has its fields.
mod header {
    pub const MAGIC: usize = 0;
    pub const VERSION: usize = 4;
    pub const SESSION: usize = 8;
    pub const TIMESTAMP: usize = 16;
    pub const NUM_QUEUES: usize = 24;
    pub const QUEUES: usize = 32;
    pub const UDATA_SIZE: usize = 5752;
    pub const UDATA: usize = 5756;
}

/// The size of `struct LGMPHeaderQueue`, and where it has its fields.
mod queue {
    pub const SIZE: usize = 1144;
    pub const ID: usize = 0;
    pub const NUM_MESSAGES: usize = 4;
    pub const NEW_SUB_COUNT: usize = 8;
    pub const MAX_TIME: usize = 12;
    pub const POSITION: usize = 16;
    pub const MESSAGES: usize = 20;
    pub const TIMEOUT: usize = 24;
    pub const CLIENT_ID: usize = 280;
    pub const LOCK: usize = 408;
    pub const SUBS: usize = 416;
    pub const START: usize = 424;
    pub const MSG_TIMEOUT: usize = 432;
    pub const COUNT: usize = 440;
    pub const CLIENT_LOCK: usize = 444;
    pub const CLIENT_AVAILABLE: usize = 448;
    pub const CLIENT_WRITE: usize = 452;
    pub const CLIENT_SENT: usize = 456;
    pub const CLIENT_RECEIVED: usize = 460;
    /// `struct LGMPClientMessage`s: a size, and the message.
    pub const CLIENT_MESSAGES: usize = 464;
    pub const CLIENT_MESSAGE: usize = 68;
}

/// The size of `struct LGMPHeaderMessage`, and where it has its fields.
mod message {
    pub const SIZE: usize = 16;
    pub const UDATA: usize = 0;
    pub const LEN: usize = 4;
    pub const OFFSET: usize = 8;
    pub const PENDING: usize = 12;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// Nothing new in the queue.
    Empty,
    /// The host has not made the queue yet.
    NoSuchQueue,
    /// No host runs: none has yet, or the one that did stopped.
    NotRunning,
    /// The host speaks another version of LGMP.
    Version,
    /// The session is over: the host restarted or stopped, or dropped this client.
    SessionOver,
    /// The host has no room for another message from its clients yet.
    Full,
    /// What the memory holds makes no sense.
    Corrupted,
}

/// Memory shared with the guest.
pub struct Shm {
    ptr: NonNull<u8>,
    len: usize,
}

impl Shm {
    /// # Safety
    ///
    /// `ptr` has to be aligned to 8 bytes, and stay mapped, readable and writable, for
    /// `len` bytes for as long as the `Shm` lives.
    pub unsafe fn new(ptr: NonNull<u8>, len: usize) -> Self {
        Self { ptr, len }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    /// The address of the `size` bytes at `offset`, when they are all in the memory and
    /// the address is a multiple of `align`.
    fn at(&self, offset: usize, size: usize, align: usize) -> Result<*mut u8, Error> {
        match offset.checked_add(size) {
            Some(end) if end <= self.len && offset.is_multiple_of(align) => {
                // SAFETY: `offset` is within the mapping.
                Ok(unsafe { self.ptr.as_ptr().add(offset) })
            }
            _ => Err(Error::Corrupted),
        }
    }

    pub fn u32(&self, offset: usize) -> Result<u32, Error> {
        let at = self.at(offset, 4, 4)?;
        // SAFETY: in the mapping and aligned; volatile, as the guest changes it at will.
        Ok(unsafe { at.cast::<u32>().read_volatile() })
    }

    fn u64(&self, offset: usize) -> Result<u64, Error> {
        let at = self.at(offset, 8, 8)?;
        // SAFETY: as in `u32`.
        Ok(unsafe { at.cast::<u64>().read_volatile() })
    }

    fn set_u32(&self, offset: usize, value: u32) -> Result<(), Error> {
        let at = self.at(offset, 4, 4)?;
        // SAFETY: as in `u32`.
        unsafe { at.cast::<u32>().write_volatile(value) };
        Ok(())
    }

    fn set_u64(&self, offset: usize, value: u64) -> Result<(), Error> {
        let at = self.at(offset, 8, 8)?;
        // SAFETY: as in `u32`.
        unsafe { at.cast::<u64>().write_volatile(value) };
        Ok(())
    }

    pub fn atomic_u32(&self, offset: usize) -> Result<&AtomicU32, Error> {
        let at = self.at(offset, 4, 4)?;
        // SAFETY: in the mapping, which outlives the borrow of `self`, and aligned. The
        // guest only ever accesses the fields LGMP makes atomic atomically, as this does.
        Ok(unsafe { AtomicU32::from_ptr(at.cast()) })
    }

    fn atomic_u64(&self, offset: usize) -> Result<&AtomicU64, Error> {
        let at = self.at(offset, 8, 8)?;
        // SAFETY: as in `atomic_u32`.
        Ok(unsafe { AtomicU64::from_ptr(at.cast()) })
    }

    fn write(&self, offset: usize, src: &[u8]) -> Result<(), Error> {
        let at = self.at(offset, src.len(), 1)?;
        // SAFETY: as in `read`, the other way.
        unsafe { std::ptr::copy_nonoverlapping(src.as_ptr(), at, src.len()) };
        Ok(())
    }

    /// Copy the bytes at `offset` into `dst`.
    pub fn read(&self, offset: usize, dst: &mut [u8]) -> Result<(), Error> {
        let at = self.at(offset, dst.len(), 1)?;
        // SAFETY: the bytes are in the mapping, which memory of this process such as `dst`
        // is not part of.
        unsafe { std::ptr::copy_nonoverlapping(at, dst.as_mut_ptr(), dst.len()) };
        Ok(())
    }
}

/// A queue's lock, held until dropped.
struct Lock<'a>(&'a AtomicU32);

impl Lock<'_> {
    fn take(lock: &AtomicU32, wait: Duration) -> Option<Lock<'_>> {
        let started = Instant::now();
        loop {
            if lock.compare_exchange(0, 1, SeqCst, SeqCst).is_ok() {
                return Some(Lock(lock));
            }
            if started.elapsed() >= wait {
                return None;
            }
            std::hint::spin_loop();
        }
    }
}

impl Drop for Lock<'_> {
    fn drop(&mut self) {
        self.0.store(0, SeqCst);
    }
}

/// The subscribers of a queue are two masks of 32 bits in one 64: the upper ones those
/// subscribed, the lower ones those the host dropped for taking too long.
fn subscribed(subs: u64) -> u32 {
    (subs >> 32) as u32
}

fn dropped(subs: u64) -> u32 {
    subs as u32
}

fn clear(subs: u64, bits: u32) -> u64 {
    subs & !(u64::from(bits) | u64::from(bits) << 32)
}

pub struct Client<'a> {
    shm: &'a Shm,
    id: u32,
    session: u32,
    /// The host's heartbeat, as last seen.
    hosttime: u64,
    /// When the heartbeat last moved.
    heartbeat: Instant,
}

impl<'a> Client<'a> {
    /// A client, `id` among the host's clients, of whatever host `shm` has, whose
    /// heartbeat it starts watching from now.
    pub fn new(shm: &'a Shm, id: u32) -> Result<Self, Error> {
        Ok(Self {
            shm,
            id,
            session: 0,
            hosttime: shm.atomic_u64(header::TIMESTAMP)?.load(SeqCst),
            heartbeat: Instant::now(),
        })
    }

    /// Join the host's session, once its heartbeat has moved since this client was made
    /// or last tried, and have what the host tells its clients.
    pub fn join(&mut self) -> Result<Vec<u8>, Error> {
        let shm = self.shm;
        if shm.u32(header::MAGIC)? != MAGIC {
            return Err(Error::NotRunning);
        }
        if shm.u32(header::VERSION)? != VERSION {
            return Err(Error::Version);
        }
        let timestamp = shm.atomic_u64(header::TIMESTAMP)?.load(SeqCst);
        if timestamp == self.hosttime {
            return Err(Error::NotRunning);
        }
        self.session = shm.u32(header::SESSION)?;
        self.hosttime = timestamp;
        self.heartbeat = Instant::now();
        let size = shm.u32(header::UDATA_SIZE)? as usize;
        if size > shm.len() {
            return Err(Error::Corrupted);
        }
        let mut udata = vec![0; size];
        shm.read(header::UDATA, &mut udata)?;
        Ok(udata)
    }

    /// Whether the session joined goes on: the host has not restarted, and its heartbeat
    /// moves.
    pub fn alive(&mut self) -> bool {
        let shm = self.shm;
        if shm.u32(header::SESSION) != Ok(self.session) {
            return false;
        }
        let Ok(timestamp) = shm.atomic_u64(header::TIMESTAMP).map(|t| t.load(SeqCst)) else {
            return false;
        };
        if timestamp != self.hosttime {
            self.hosttime = timestamp;
            self.heartbeat = Instant::now();
            return true;
        }
        self.heartbeat.elapsed() <= HEARTBEAT_TIMEOUT
    }

    /// Take the next free place among the subscribers of the queue `queue_id`.
    pub fn subscribe(&self, queue_id: u32) -> Result<Queue, Error> {
        let shm = self.shm;
        let count = shm.u32(header::NUM_QUEUES)?.min(MAX_QUEUES) as usize;
        let base = (0..count)
            .map(|i| header::QUEUES + i * queue::SIZE)
            .find(|&base| shm.u32(base + queue::ID) == Ok(queue_id))
            .ok_or(Error::NoSuchQueue)?;
        let _lock = Lock::take(shm.atomic_u32(base + queue::LOCK)?, LOCK_TIMEOUT)
            .ok_or(Error::SessionOver)?;
        let subs_at = shm.atomic_u64(base + queue::SUBS)?;
        let mut subs = subs_at.load(SeqCst);
        // Places of subscribers the host dropped long enough ago can be had again.
        if subscribed(subs) != 0 {
            let hosttime = shm.atomic_u64(header::TIMESTAMP)?.load(SeqCst);
            let mut reap = 0;
            for id in 0..32 {
                let bit = 1u32 << id;
                if dropped(subs) & bit != 0 && hosttime > shm.u64(base + queue::TIMEOUT + 8 * id)? {
                    reap |= bit;
                    shm.set_u64(base + queue::TIMEOUT + 8 * id, 0)?;
                    shm.set_u32(base + queue::CLIENT_ID + 4 * id, 0)?;
                }
            }
            subs = clear(subs, reap);
        }
        let taken = subscribed(subs) | dropped(subs);
        let id = (0..32)
            .find(|id| taken & (1 << id) == 0)
            .ok_or(Error::SessionOver)?;
        shm.set_u64(base + queue::TIMEOUT + 8 * id, 0)?;
        shm.set_u32(base + queue::CLIENT_ID + 4 * id, self.id)?;
        subs_at.store(subs | 1 << (32 + id), SeqCst);
        shm.atomic_u32(base + queue::NEW_SUB_COUNT)?
            .fetch_add(1, SeqCst);
        Ok(Queue {
            base,
            id,
            position: shm.atomic_u32(base + queue::POSITION)?.load(SeqCst),
        })
    }
}

/// A message in a queue: `size` bytes at `offset` in the memory, and a number the
/// application gives its own meaning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Message {
    pub udata: u32,
    pub offset: usize,
    pub size: usize,
}

/// A client's place among a queue's subscribers.
pub struct Queue {
    /// Where the queue's header is.
    base: usize,
    /// The client's place.
    id: usize,
    /// The message the client is at.
    position: u32,
}

impl Queue {
    fn bit(&self) -> u32 {
        1 << self.id
    }

    fn check(&self, shm: &Shm) -> Result<(), Error> {
        let subs = shm.atomic_u64(self.base + queue::SUBS)?.load(SeqCst);
        if dropped(subs) & self.bit() != 0 || subscribed(subs) & self.bit() == 0 {
            return Err(Error::SessionOver);
        }
        if shm.atomic_u32(self.base + queue::POSITION)?.load(SeqCst) == self.position {
            return Err(Error::Empty);
        }
        Ok(())
    }

    /// Where the header of the message at `self.position` is, and how many the queue has.
    fn header(&self, shm: &Shm) -> Result<(usize, u32), Error> {
        let count = shm.u32(self.base + queue::NUM_MESSAGES)?;
        if self.position >= count {
            return Err(Error::Corrupted);
        }
        let messages = shm.u32(self.base + queue::MESSAGES)? as usize;
        let at = messages
            .checked_add(self.position as usize * message::SIZE)
            .ok_or(Error::Corrupted)?;
        Ok((at, count))
    }

    /// The next message, which the host leaves be until [`Queue::done`].
    pub fn peek(&self, client: &Client) -> Result<Message, Error> {
        let shm = client.shm;
        self.check(shm)?;
        let (at, _) = self.header(shm)?;
        let message = Message {
            udata: shm.u32(at + message::UDATA)?,
            size: shm.u32(at + message::LEN)? as usize,
            offset: shm.u32(at + message::OFFSET)? as usize,
        };
        shm.at(message.offset, message.size, 1)?;
        Ok(message)
    }

    /// Be done with the message [`Queue::peek`] gave, and move on to the next.
    pub fn done(&mut self, client: &Client) -> Result<(), Error> {
        let shm = client.shm;
        self.check(shm)?;
        let (at, count) = self.header(shm)?;
        let bit = self.bit();
        // The last subscriber done with the message takes it off the queue.
        let pending = shm.atomic_u32(at + message::PENDING)?;
        if pending.fetch_and(!bit, SeqCst) & !bit == 0
            && let Some(_lock) =
                Lock::take(shm.atomic_u32(self.base + queue::LOCK)?, Duration::ZERO)
            && shm.u32(self.base + queue::START)? == self.position
        {
            shm.set_u32(self.base + queue::START, (self.position + 1) % count)?;
            let queued = shm.atomic_u32(self.base + queue::COUNT)?;
            if queued.fetch_sub(1, SeqCst) == 0 {
                queued.store(0, SeqCst);
                return Err(Error::Corrupted);
            }
            let timestamp = shm.atomic_u64(header::TIMESTAMP)?.load(SeqCst);
            let max_time = shm.u32(self.base + queue::MAX_TIME)?;
            shm.atomic_u64(self.base + queue::MSG_TIMEOUT)?
                .store(timestamp.wrapping_add(u64::from(max_time)), SeqCst);
        }
        self.position = (self.position + 1) % count;
        Ok(())
    }

    /// Send the host `message`, of at most 64 bytes; its number, which
    /// [`Queue::received`] reaches once the host has taken it.
    pub fn send(&self, client: &Client, message: &[u8]) -> Result<u32, Error> {
        assert!(
            message.len() <= CLIENT_MESSAGE_SIZE,
            "a message of 64 bytes at most"
        );
        let shm = client.shm;
        if dropped(shm.atomic_u64(self.base + queue::SUBS)?.load(SeqCst)) & self.bit() != 0 {
            return Err(Error::SessionOver);
        }
        let available = shm.atomic_u32(self.base + queue::CLIENT_AVAILABLE)?;
        if available.load(SeqCst) == 0 {
            return Err(Error::Full);
        }
        let _lock = Lock::take(
            shm.atomic_u32(self.base + queue::CLIENT_LOCK)?,
            LOCK_TIMEOUT,
        )
        .ok_or(Error::SessionOver)?;
        if available.load(SeqCst) == 0 {
            return Err(Error::Full);
        }
        let write = shm.atomic_u32(self.base + queue::CLIENT_WRITE)?;
        let position = write.load(SeqCst);
        if position >= CLIENT_MESSAGES {
            return Err(Error::Corrupted);
        }
        let at = self.base + queue::CLIENT_MESSAGES + position as usize * queue::CLIENT_MESSAGE;
        shm.set_u32(at, message.len() as u32)?;
        shm.write(at + 4, message)?;
        write.store((position + 1) % CLIENT_MESSAGES, SeqCst);
        available.fetch_sub(1, SeqCst);
        let sent = shm.atomic_u32(self.base + queue::CLIENT_SENT)?;
        Ok(sent.fetch_add(1, SeqCst).wrapping_add(1))
    }

    /// How many messages from its clients the host has taken.
    pub fn received(&self, client: &Client) -> Result<u32, Error> {
        Ok(client
            .shm
            .atomic_u32(self.base + queue::CLIENT_RECEIVED)?
            .load(SeqCst))
    }

    /// Give the place up.
    pub fn unsubscribe(self, client: &Client) {
        let shm = client.shm;
        let Ok(lock) = shm.atomic_u32(self.base + queue::LOCK) else {
            return;
        };
        let Some(_lock) = Lock::take(lock, LOCK_TIMEOUT) else {
            return;
        };
        let Ok(subs_at) = shm.atomic_u64(self.base + queue::SUBS) else {
            return;
        };
        let subs = subs_at.load(SeqCst);
        if dropped(subs) & self.bit() != 0 {
            return;
        }
        subs_at.store(clear(subs, self.bit()), SeqCst);
        let _ = shm.set_u64(self.base + queue::TIMEOUT + 8 * self.id, 0);
        let _ = shm.set_u32(self.base + queue::CLIENT_ID + 4 * self.id, 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 64 KiB laid out as an LGMP host lays it out, at the offsets gcc gives LGMP 6's
    /// headers, written here apart from the constants above so that they check those.
    struct Host(Vec<u64>);

    impl Host {
        fn new() -> Self {
            let mut host = Self(vec![0; 8192]);
            host.set(0, 0x504d_474c);
            host.set(4, 6);
            host.set(8, 7);
            host.set(16, 1);
            host.set(24, 1);
            // One queue, of id 2 and two messages, whose headers are at 8192.
            host.set(32, 2);
            host.set(36, 2);
            host.set(44, 1000);
            host.set(52, 8192);
            host.set(480, 10);
            host.set(5752, 3);
            host.set(5756, u32::from_le_bytes(*b"abc\0"));
            host
        }

        fn set(&mut self, offset: usize, value: u32) {
            let bytes = as_bytes(&mut self.0);
            bytes[offset..offset + 4].copy_from_slice(&value.to_ne_bytes());
        }

        fn get(&mut self, offset: usize) -> u32 {
            let bytes = as_bytes(&mut self.0);
            u32::from_ne_bytes(bytes[offset..offset + 4].try_into().unwrap())
        }

        fn shm(&mut self) -> Shm {
            let ptr = NonNull::new(self.0.as_mut_ptr().cast()).unwrap();
            // SAFETY: the vector outlives the `Shm` in each test, and its u64 align it.
            unsafe { Shm::new(ptr, self.0.len() * 8) }
        }

        /// Post message `index`, of `size` bytes at `offset`, to the first subscriber.
        fn post(&mut self, index: u32, offset: u32, size: u32) {
            let at = 8192 + 16 * index as usize;
            self.set(at, 4);
            self.set(at + 4, size);
            self.set(at + 8, offset);
            self.set(at + 12, 1);
            self.set(48, (index + 1) % 2);
            let count = self.get(472);
            self.set(472, count + 1);
        }
    }

    fn as_bytes(words: &mut [u64]) -> &mut [u8] {
        // SAFETY: any bytes are valid u8, and u64 has no padding.
        unsafe { std::slice::from_raw_parts_mut(words.as_mut_ptr().cast(), words.len() * 8) }
    }

    #[test]
    fn a_client_joins_once_the_heartbeat_moves() {
        let mut host = Host::new();
        let shm = host.shm();
        let mut client = Client::new(&shm, 5).unwrap();
        assert_eq!(client.join(), Err(Error::NotRunning));
        host.set(16, 2);
        let mut client = Client::new(&shm, 5).unwrap();
        host.set(16, 3);
        assert_eq!(client.join().unwrap(), b"abc");
        assert!(client.alive());
        host.set(8, 8);
        assert!(!client.alive(), "the host restarted");
    }

    #[test]
    fn other_versions_are_not_joined() {
        let mut host = Host::new();
        host.set(4, 5);
        let shm = host.shm();
        let mut client = Client::new(&shm, 5).unwrap();
        host.set(16, 2);
        assert_eq!(client.join(), Err(Error::Version));
    }

    #[test]
    fn a_subscriber_takes_messages_in_turn() {
        let mut host = Host::new();
        let shm = host.shm();
        let client = Client::new(&shm, 5).unwrap();
        assert!(matches!(client.subscribe(9), Err(Error::NoSuchQueue)));
        let mut queue = client.subscribe(2).unwrap();
        assert_eq!(host.get(452), 1, "subscribed, as the first");
        assert_eq!(host.get(312), 5, "its client id");
        assert_eq!(host.get(40), 1, "one new subscriber");
        assert_eq!(queue.peek(&client), Err(Error::Empty));

        host.post(0, 16384, 16);
        let message = queue.peek(&client).unwrap();
        assert_eq!(
            message,
            Message {
                udata: 4,
                offset: 16384,
                size: 16
            }
        );
        queue.done(&client).unwrap();
        assert_eq!(host.get(8204), 0, "no longer pending");
        assert_eq!(host.get(456), 1, "off the queue");
        assert_eq!(host.get(472), 0);
        assert_eq!(queue.peek(&client), Err(Error::Empty));

        host.post(1, 65530, 16);
        assert_eq!(
            queue.peek(&client),
            Err(Error::Corrupted),
            "beyond the memory"
        );

        queue.unsubscribe(&client);
        assert_eq!(host.get(452), 0);
        assert_eq!(host.get(312), 0);
    }

    #[test]
    fn clients_send_the_host_messages_while_it_has_room() {
        let mut host = Host::new();
        let shm = host.shm();
        let client = Client::new(&shm, 5).unwrap();
        let queue = client.subscribe(2).unwrap();
        assert_eq!(queue.send(&client, b"hello"), Ok(1));
        assert_eq!(host.get(496), 5, "its size");
        assert_eq!(&host.get(500).to_le_bytes(), b"hell");
        assert_eq!(host.get(480), 9, "room for nine more");
        assert_eq!(host.get(484), 1, "written up to the second");
        assert_eq!(queue.received(&client), Ok(0));
        host.set(492, 1);
        assert_eq!(queue.received(&client), Ok(1));
        host.set(480, 0);
        assert_eq!(queue.send(&client, b"again"), Err(Error::Full));
        host.set(480, 1);
        host.set(484, 12);
        assert_eq!(queue.send(&client, b"again"), Err(Error::Corrupted));
    }

    #[test]
    fn a_dropped_subscriber_is_told() {
        let mut host = Host::new();
        let shm = host.shm();
        let client = Client::new(&shm, 5).unwrap();
        let queue = client.subscribe(2).unwrap();
        host.post(0, 16384, 16);
        host.set(448, 1);
        assert_eq!(queue.peek(&client), Err(Error::SessionOver));
    }
}
