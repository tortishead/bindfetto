#![no_std]

//! Shared data contract between the eBPF probe and the userspace consumer.
//!
//! This is the wire format that crosses the ring buffer. Keep it `#[repr(C)]`,
//! `Copy`, and free of pointers/padding surprises so both sides agree byte-for-byte.

/// `TF_ONE_WAY` — set in [`TxEvent::flags`] for async (oneway) transactions.
pub const TF_ONE_WAY: u32 = 0x01;

/// Max bytes of UTF-16LE interface descriptor captured; longer names truncate.
/// 256 bytes = 128 UTF-16 code units, enough for long names like
/// `ICarWatchdogServiceForSystem` that overflowed the original 128.
pub const MAX_IFACE_BYTES: usize = 256;

/// Header magic that `Parcel::writeInterfaceToken` writes (`B_PACK_CHARS('S','Y',
/// 'S','T')`) read back as a little-endian u32. Its presence at parcel offset 8
/// marks a transaction that begins with an interface descriptor.
pub const IFACE_HEADER_MAGIC: u32 = 0x5359_5354;

/// Key for the in-kernel interface filter map (`WANTED`): a zero-padded UTF-16LE
/// interface descriptor, byte-identical to the bytes the probe captures into
/// [`TxEvent::iface`]. Using the full descriptor as the key is collision-free, so the
/// probe needs no hashing on the hot path — it just looks the captured bytes up
/// directly. The userspace side builds the same key by UTF-16LE-encoding the wanted
/// interface name and zero-padding to [`MAX_IFACE_BYTES`].
#[repr(transparent)]
#[derive(Clone, Copy)]
pub struct IfaceKey(pub [u8; MAX_IFACE_BYTES]);

/// One captured Binder transaction.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct TxEvent {
    /// Kernel monotonic timestamp (ns) when the transaction was observed.
    pub ts_ns: u64,
    /// Sender process id (tgid).
    pub src_pid: u32,
    /// Sender thread id.
    pub src_tid: u32,
    /// Target process id (`to_proc` from the tracepoint).
    pub dst_pid: u32,
    /// Raw transaction code (method selector; decoded offline via the catalog).
    pub code: u32,
    /// Transaction flags; test against [`TF_ONE_WAY`] for async.
    pub flags: u32,
    /// Non-zero if this is a reply (replies carry no interface descriptor).
    pub reply: u32,
    /// Parcel payload size in bytes.
    pub data_size: u32,
    /// Valid bytes in [`iface`] (UTF-16LE); 0 when the transaction carries no
    /// interface descriptor (replies, special transactions, unreadable buffer).
    pub iface_byte_len: u32,
    /// Interface descriptor as raw UTF-16LE bytes, decoded by the consumer.
    pub iface: [u8; MAX_IFACE_BYTES],
}

impl TxEvent {
    /// True if this is an async (oneway) transaction.
    #[inline]
    pub fn is_oneway(&self) -> bool {
        self.flags & TF_ONE_WAY != 0
    }
}

#[cfg(feature = "user")]
unsafe impl aya::Pod for TxEvent {}

#[cfg(feature = "user")]
unsafe impl aya::Pod for IfaceKey {}
