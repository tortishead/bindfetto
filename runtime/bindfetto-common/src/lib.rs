// `no_std` for the probe/consumer builds; the test build pulls in `std` for the harness.
#![cfg_attr(not(test), no_std)]

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

/// Default runtime cap on raw parcel bytes captured per transaction (M6), used unless
/// the operator raises it (`--parcel-max` / `PARCEL max`). Small so casual capture is
/// cheap; captured from parcel offset 0 (head + body), so the offline reader
/// reconstructs descriptor → header → args; bytes past the cap are lost (truncated).
pub const PARCEL_CAP_DEFAULT: u32 = 256;

/// Hard compile-time ceiling on the parcel cap. The probe stages the payload in a
/// per-CPU scratch map before a variable-length ring write, and the kernel caps a
/// per-CPU map value at `PCPU_MIN_UNIT_SIZE` (32 KiB); 30 KiB leaves room for the
/// [`TxRecord`] header so the scratch value stays under that limit. The runtime cap is
/// clamped to this, and it's the constant the verifier uses to bound the payload read.
pub const PARCEL_CEILING: usize = 30 * 1024;

/// Header magic that `Parcel::writeInterfaceToken` writes (`B_PACK_CHARS('S','Y',
/// 'S','T')`) read back as a little-endian u32. Its presence at parcel offset 8
/// marks a transaction that begins with an interface descriptor.
pub const IFACE_HEADER_MAGIC: u32 = 0x5359_5354;

// Binder return protocol error codes we surface (the second attach point, M5).
// These are the `cmd` values carried by the `binder:binder_return` tracepoint —
// `_IO('r', n)` = `0x7200 | n`, matching the low byte the kernel uses as an index into
// its `binder_return_strings` table. Only these two are real transaction *failures*;
// `BR_FROZEN_REPLY` is the frozen-target variant of a failed reply (kernels ≥ 5.15).
/// `BR_DEAD_REPLY` — the target of a transaction died before it could reply.
pub const BR_DEAD_REPLY: u32 = 0x7205; // _IO('r', 5)
/// `BR_FAILED_REPLY` — the transaction failed (security denial, bad handle, oversized …).
pub const BR_FAILED_REPLY: u32 = 0x7211; // _IO('r', 17)
/// `BR_FROZEN_REPLY` — the target process was frozen (cached), so the reply failed.
pub const BR_FROZEN_REPLY: u32 = 0x7212; // _IO('r', 18)

/// True for the binder return `cmd` values bindfetto reports as transaction errors.
#[inline]
pub fn is_error_return(cmd: u32) -> bool {
    matches!(cmd, BR_DEAD_REPLY | BR_FAILED_REPLY | BR_FROZEN_REPLY)
}

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
    /// 0 for a normal transaction; otherwise the binder return `cmd` of a captured
    /// error (`BR_FAILED_REPLY`/`BR_DEAD_REPLY`/`BR_FROZEN_REPLY`). For an error event
    /// the src/dst/code/iface describe the *failing* transaction, correlated per-thread.
    pub err_code: u32,
    /// The binder transaction `debug_id` from the tracepoint — a per-transaction id the
    /// kernel also records in its `failed_transaction_log`. The consumer matches an error
    /// event against that log by `debug_id` to recover the *concrete* failure errno
    /// (e.g. `-ENOSPC` = target buffer full) that the coarse `BR_*` code alone doesn't
    /// carry. Also keeps the post-`ts_ns` u32 count even so the struct has no *implicit*
    /// tail padding — the probe copies the whole struct into the ring buffer and the BPF
    /// verifier rejects reading uninitialized padding bytes.
    pub debug_id: i32,
    /// Valid bytes in [`iface`] (UTF-16LE); 0 when the transaction carries no
    /// interface descriptor (replies, special transactions, unreadable buffer).
    pub iface_byte_len: u32,
    /// Interface descriptor as raw UTF-16LE bytes, decoded by the consumer.
    pub iface: [u8; MAX_IFACE_BYTES],
    /// The sender's `comm` (kernel task name, NUL-padded, ≤ 15 chars) captured *in the
    /// probe* via `bpf_get_current_comm()` while the sender is alive mid-transaction.
    /// Lets the consumer name a short-lived sender that has already exited by the time it
    /// reads `/proc/<pid>` — the exit race the userspace resolver can't win. All-zero when
    /// unavailable (e.g. error events reconstructed from a correlated record).
    pub src_comm: [u8; 16],
}

impl TxEvent {
    /// True if this is an async (oneway) transaction.
    #[inline]
    pub fn is_oneway(&self) -> bool {
        self.flags & TF_ONE_WAY != 0
    }

    /// True if this event is a captured transaction error rather than a transaction.
    #[inline]
    pub fn is_error(&self) -> bool {
        self.err_code != 0
    }
}

/// A [`TxEvent`] plus a captured slice of the raw parcel (M6). Staged in the probe's
/// per-CPU scratch map, then written to the ring buffer as a **variable-length** record
/// (`ev` + `parcel_len` + exactly `parcel_len` payload bytes) *instead of* a bare
/// `TxEvent` — so the ring only pays for the bytes actually captured, and a big cap
/// costs nothing when small parcels flow. The consumer tells a record from a bare
/// `TxEvent` by the ring item's byte length, so the no-parcel path is byte-identical.
///
/// `parcel` is sized to the compile-time [`PARCEL_CEILING`]; only the first `parcel_len`
/// bytes are ever written or emitted.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct TxRecord {
    /// The transaction header — identical to what the no-parcel path emits.
    pub ev: TxEvent,
    /// Valid bytes in [`parcel`]; `<= PARCEL_CEILING`. Captured from parcel offset 0.
    pub parcel_len: u32,
    /// Raw parcel bytes (head + body), decoded offline against the catalog's arg types.
    pub parcel: [u8; PARCEL_CEILING],
}

#[cfg(feature = "user")]
unsafe impl aya::Pod for TxEvent {}

#[cfg(feature = "user")]
unsafe impl aya::Pod for TxRecord {}

#[cfg(feature = "user")]
unsafe impl aya::Pod for IfaceKey {}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::{align_of, offset_of, size_of};

    // The probe copies whole `#[repr(C)]` structs into BPF maps / the ring buffer, and
    // the verifier rejects reads of uninitialized padding — so any *implicit* tail or
    // interior padding is a load-time failure (it bit M5 and M6 twice). These assert the
    // structs are gap-free by checking the size equals the sum of the field sizes.

    #[test]
    fn tx_event_has_no_padding() {
        // u64 + ten u32 + [u8; 256] + [u8; 16].
        assert_eq!(size_of::<TxEvent>(), 8 + 10 * 4 + MAX_IFACE_BYTES + 16);
        assert_eq!(align_of::<TxEvent>(), 8);
        // 8-aligned size (the post-`ts_ns` u32 count is kept even on purpose).
        assert_eq!(size_of::<TxEvent>() % 8, 0);
    }

    #[test]
    fn tx_record_layout_matches_consumer_reader() {
        // The consumer reads `parcel_len` at `size_of::<TxEvent>()` and the payload at
        // `offset_of!(TxRecord, parcel)`; these must hold or it decodes garbage.
        assert_eq!(offset_of!(TxRecord, ev), 0);
        assert_eq!(offset_of!(TxRecord, parcel_len), size_of::<TxEvent>());
        assert_eq!(offset_of!(TxRecord, parcel), size_of::<TxEvent>() + 4);
        // No interior padding between the header and the payload array.
        assert_eq!(offset_of!(TxRecord, parcel), 8 + 10 * 4 + MAX_IFACE_BYTES + 16 + 4);
    }

    #[test]
    fn parcel_scratch_fits_the_per_cpu_limit() {
        // The probe stages a whole `TxRecord` in a per-CPU map, whose value the kernel
        // caps at PCPU_MIN_UNIT_SIZE (32 KiB). The ceiling must leave room for the header.
        assert!(size_of::<TxRecord>() <= 32 * 1024);
        assert_eq!(PARCEL_CEILING, 30 * 1024);
    }

    #[test]
    fn iface_key_is_transparent_over_the_descriptor() {
        // The probe reinterprets the captured descriptor bytes as an IfaceKey in place;
        // that's only sound if they're the same size and alignment.
        assert_eq!(size_of::<IfaceKey>(), MAX_IFACE_BYTES);
        assert_eq!(align_of::<IfaceKey>(), 1);
    }

    #[test]
    fn oneway_and_error_predicates() {
        let mut ev: TxEvent = unsafe { core::mem::zeroed() };
        assert!(!ev.is_oneway() && !ev.is_error());
        ev.flags = TF_ONE_WAY;
        assert!(ev.is_oneway());
        ev.err_code = BR_FAILED_REPLY;
        assert!(ev.is_error());
    }

    #[test]
    fn error_return_codes_classified() {
        for c in [BR_DEAD_REPLY, BR_FAILED_REPLY, BR_FROZEN_REPLY] {
            assert!(is_error_return(c));
        }
        // A normal reply / an unrelated return cmd is not an error.
        assert!(!is_error_return(0));
        assert!(!is_error_return(0x7200)); // BR_ERROR base, not one we surface
    }

    #[test]
    fn header_magic_matches_b_pack_chars() {
        // B_PACK_CHARS('S','Y','S','T') = (S<<24)|(Y<<16)|(S<<8)|T as a u32 fourcc.
        let packed = ((b'S' as u32) << 24) | ((b'Y' as u32) << 16) | ((b'S' as u32) << 8) | b'T' as u32;
        assert_eq!(IFACE_HEADER_MAGIC, packed);
        assert_eq!(IFACE_HEADER_MAGIC, 0x5359_5354);
    }

    #[test]
    fn default_cap_within_ceiling() {
        assert!(PARCEL_CAP_DEFAULT as usize <= PARCEL_CEILING);
    }
}
