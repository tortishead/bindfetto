#![no_std]
#![no_main]

//! Bindfetto probe (M1–M5).
//!
//! Three attach points, correlated per-thread:
//!
//! * **kprobe on `binder_transaction()`** — runs at function entry, reads the
//!   parcel size and the interface descriptor from the `binder_transaction_data`
//!   argument, and stashes them keyed by pid_tgid.
//! * **tracepoint `binder:binder_transaction`** — runs later inside the same call,
//!   reads the target pid / code / flags, pulls the stashed size+descriptor, and
//!   emits a [`TxEvent`] to the ring buffer.
//! * **tracepoint `binder:binder_return`** — the return/error path (M5). When error
//!   capture is enabled it watches for `BR_FAILED_REPLY`/`BR_DEAD_REPLY`/
//!   `BR_FROZEN_REPLY` and emits an error [`TxEvent`], correlating it to the calling
//!   thread's last captured transaction via [`LAST_TX`].

use aya_ebpf::{
    helpers::{
        bpf_get_current_pid_tgid, bpf_ktime_get_ns, bpf_probe_read_kernel, bpf_probe_read_user,
        bpf_probe_read_user_buf,
    },
    macros::{kprobe, map, tracepoint},
    maps::{Array, HashMap, PerCpuArray, RingBuf},
    programs::{ProbeContext, TracePointContext},
};
use bindfetto_common::{
    is_error_return, IfaceKey, TxEvent, TxRecord, IFACE_HEADER_MAGIC, MAX_IFACE_BYTES,
    PARCEL_CEILING,
};

/// Ring buffer to userspace. Sized for the larger `TxRecord` (header + up to
/// `PARCEL_CAP` payload bytes) emitted when parcel capture is on (M6), so the drop rate
/// stays low; a bare `TxEvent` uses only its own bytes of the same ring.
#[map]
static EVENTS: RingBuf = RingBuf::with_byte_size(1024 * 1024, 0);

/// Per-thread hand-off from the kprobe to the tracepoint, keyed by pid_tgid.
#[map]
static STASH: HashMap<u64, Stash> = HashMap::with_max_entries(10240, 0);

/// In-kernel interface filter: descriptors the operator asked to keep. Populated from
/// userspace (`--iface`). When [`FILTER_ON`] is set, a transaction is dropped **before**
/// the ring buffer unless its captured descriptor is a key here — cutting the probe's
/// output volume and observer effect on the traced device.
#[map]
static WANTED: HashMap<IfaceKey, u8> = HashMap::with_max_entries(256, 0);

/// One-element enable flag for the interface filter (element 0: 0 = off, non-0 = on).
/// A map (not a load-time const) so the control app can toggle filtering at runtime.
#[map]
static FILTER_ON: Array<u32> = Array::with_max_entries(1, 0);

/// One-element enable flag for error capture (element 0: 0 = off, non-0 = on). Off by
/// default (per SPEC); toggled by `--errors` / the control app. Gates both the
/// `binder_return` attach point and the per-thread [`LAST_TX`] bookkeeping.
#[map]
static ERRORS_ON: Array<u32> = Array::with_max_entries(1, 0);

/// One-element flag: when non-0, normal (successful) replies are captured too instead of
/// being dropped before the ring buffer. Set once at startup from `--include-replies`.
#[map]
static INCLUDE_REPLIES: Array<u32> = Array::with_max_entries(1, 0);

/// One-element enable flag for parcel payload capture (element 0: 0 = off, non-0 = on).
/// Off by default (M6). When on, a kept transaction's raw parcel bytes (offset 0, up to
/// [`PARCEL_MAX`]) are staged in [`PARCEL_SCRATCH`] and written to the ring buffer as a
/// variable-length [`TxRecord`]. Userspace only sets it while the interface filter is
/// active, so capture is bounded to the selected interfaces — the probe just reads the
/// flag on the hot path.
#[map]
static PARCEL_ON: Array<u32> = Array::with_max_entries(1, 0);

/// One-element runtime cap (bytes) on the captured payload, tunable live via
/// `--parcel-max` / `PARCEL max`. Clamped by userspace to [`PARCEL_CEILING`]; the probe
/// clamps again against that constant so the verifier can bound the read. 0 falls back
/// to no capture (treated as off).
#[map]
static PARCEL_MAX: Array<u32> = Array::with_max_entries(1, 0);

/// Per-CPU scratch to assemble a [`TxRecord`] before a variable-length ring write. Lives
/// in a map (not on the 512-byte BPF stack, and not shared across CPUs) because the
/// payload can be up to [`PARCEL_CEILING`]. The kernel caps a per-CPU map value at 32 KiB,
/// which is why the ceiling sits below that.
#[map]
static PARCEL_SCRATCH: PerCpuArray<TxRecord> = PerCpuArray::with_max_entries(1, 0);

/// Per-thread copy of the last captured (kept) outgoing transaction, keyed by pid_tgid.
/// Written by the transaction tracepoint only while error capture is on, and read by the
/// `binder_return` attach point to correlate a `BR_*_REPLY` failure back to the
/// transaction that provoked it (same thread: the sender's `BINDER_WRITE_READ` does the
/// write then reads the error). Distinct from [`STASH`], which the transaction tracepoint
/// consumes-and-removes; this one persists across the return path. Stores the whole
/// [`TxEvent`] so it can be inserted by reference — building a separate correlation struct
/// would put a second 256-byte descriptor on the 512-byte BPF stack.
#[map]
static LAST_TX: HashMap<u64, TxEvent> = HashMap::with_max_entries(10240, 0);

#[repr(C)]
#[derive(Clone, Copy)]
struct Stash {
    /// Sender-side (untagged) userspace pointer to the parcel buffer, handed to the
    /// tracepoint so it can copy the raw payload straight into the ring slot (M6) — the
    /// bytes never touch the BPF stack. 0 when the kprobe couldn't read it. Kept first
    /// (u64) so the trailing u32s leave no interior padding.
    buf_ptr: u64,
    data_size: u32,
    iface_byte_len: u32,
    /// 1 = emit this transaction, 0 = drop it (interface filter decided in the kprobe,
    /// where the descriptor is available; the tracepoint enforces it).
    keep: u32,
    /// Explicit, zero-initialized padding: the kprobe copies the whole `Stash` into the
    /// `STASH` map and the verifier rejects a map read that touches *uninitialized* tail
    /// padding. Three u32s after the u64 would leave a 4-byte implicit gap before `iface`;
    /// naming it and zeroing it makes every byte defined.
    _pad: u32,
    iface: [u8; MAX_IFACE_BYTES],
}

// --- binder:binder_transaction tracepoint field offsets (from the format file) ---
const OFF_DEBUG_ID: usize = 8;
const OFF_TO_PROC: usize = 16;
const OFF_REPLY: usize = 24;
const OFF_CODE: usize = 28;
const OFF_FLAGS: usize = 32;

// --- binder:binder_return tracepoint field offset (from the format file) ---
const OFF_RETURN_CMD: usize = 8; // uint32_t cmd

// --- struct binder_transaction_data offsets (UAPI, 64-bit) ---
const TR_DATA_SIZE: usize = 32; // binder_size_t data_size
const TR_BUFFER_PTR: usize = 48; // data.ptr.buffer (user pointer)

// --- parcel head layout written by writeInterfaceToken ---
const P_MAGIC: usize = 8; // int32 kHeader ('SYST')
const P_STRLEN: usize = 12; // int32 length in UTF-16 code units
const P_STR: usize = 16; // start of the UTF-16LE descriptor

#[kprobe]
pub fn binder_transaction_enter(ctx: ProbeContext) -> u32 {
    let _ = try_kprobe(&ctx);
    0
}

fn try_kprobe(ctx: &ProbeContext) -> Result<(), i64> {
    // arg3 = int reply. Unless `--include-replies` is set, the tracepoint drops replies,
    // so skip all their work here too (user-memory reads + stash insert) — replies are
    // ~half of all traffic. Truncate to u32: the ABI doesn't guarantee the register's
    // upper bits for int.
    let reply: usize = ctx.arg(3).ok_or(1i64)?;
    let is_reply = reply as u32 != 0;
    let include_replies = INCLUDE_REPLIES.get(0).copied().unwrap_or(0) != 0;
    if is_reply && !include_replies {
        return Ok(());
    }

    // arg2 = struct binder_transaction_data *tr (kernel pointer to a copied-in struct)
    let tr: usize = ctx.arg(2).ok_or(1i64)?;

    let data_size = unsafe { bpf_probe_read_kernel((tr + TR_DATA_SIZE) as *const u64) }? as u32;
    // data.ptr.buffer is a sender-side userspace pointer. On AArch64 it may carry a tag
    // in the top byte (TBI is always on; ARM MTE puts a logical tag in bits 59-56).
    // Strip the top byte before pointer arithmetic + user reads so we don't depend on a
    // given kernel untagging it for us; user VAs live in TTBR0 low range, so the top
    // byte is purely tag and this is a no-op on untagged pointers.
    let buf_ptr =
        (unsafe { bpf_probe_read_kernel((tr + TR_BUFFER_PTR) as *const u64) }? as usize)
            & 0x00ff_ffff_ffff_ffff;

    let mut stash = Stash {
        buf_ptr: buf_ptr as u64,
        data_size,
        iface_byte_len: 0,
        keep: 1,
        _pad: 0,
        iface: [0u8; MAX_IFACE_BYTES],
    };

    // The parcel data lives in the sender's userspace at buf_ptr. If it starts with
    // an interface token, offset 8 holds the 'SYST' magic. A parcel smaller than the
    // token header (16 bytes) can't carry one — don't even read the magic. Replies
    // (only reachable here under `--include-replies`) never carry a token, so skip it.
    if !is_reply && data_size as usize >= P_STR {
        let magic = unsafe { bpf_probe_read_user((buf_ptr + P_MAGIC) as *const u32) }.unwrap_or(0);
        if magic == IFACE_HEADER_MAGIC {
            let units =
                unsafe { bpf_probe_read_user((buf_ptr + P_STRLEN) as *const u32) }.unwrap_or(0);
            // Read only the descriptor's own bytes, clamped to our buffer and to the
            // parcel payload: a fixed MAX_IFACE_BYTES read could cross into an unmapped
            // page past a short parcel and fail, losing a perfectly valid descriptor.
            //
            // Verifier note: clamp BOTH candidates against the constant MAX first.
            // A reg-vs-reg min doesn't propagate bounds to the selected register, so
            // min(unbounded, bounded) is rejected as "unbounded memory access".
            let want = core::cmp::min((units as usize).saturating_mul(2), MAX_IFACE_BYTES);
            let avail = core::cmp::min(data_size as usize - P_STR, MAX_IFACE_BYTES);
            let nbytes = core::cmp::min(want, avail);
            if nbytes > 0
                && unsafe {
                    bpf_probe_read_user_buf(
                        (buf_ptr + P_STR) as *const u8,
                        &mut stash.iface[..nbytes],
                    )
                }
                .is_ok()
            {
                stash.iface_byte_len = nbytes as u32;
            }
        }
    }

    // Interface filter (M4): when enabled, keep only transactions whose captured
    // descriptor is in WANTED. The full descriptor is the map key, so no hashing is
    // needed on the hot path. Tokenless / unreadable transactions have an all-zero
    // key that userspace never inserts, so they drop while filtering is on.
    if FILTER_ON.get(0).copied().unwrap_or(0) != 0 {
        // Reinterpret the captured bytes as the key in place — IfaceKey is
        // repr(transparent) over [u8; MAX_IFACE_BYTES], so this avoids a second
        // 256-byte copy that would blow the 512-byte BPF stack.
        let key = unsafe { &*(stash.iface.as_ptr() as *const IfaceKey) };
        stash.keep = unsafe { WANTED.get(key).is_some() } as u32;
    }

    let pid_tgid = bpf_get_current_pid_tgid();
    let _ = STASH.insert(&pid_tgid, &stash, 0);
    Ok(())
}

#[tracepoint(category = "binder", name = "binder_transaction")]
pub fn binder_transaction(ctx: TracePointContext) -> u32 {
    match try_tracepoint(&ctx) {
        Ok(()) => 0,
        Err(_) => 1,
    }
}

fn try_tracepoint(ctx: &TracePointContext) -> Result<(), i64> {
    let pid_tgid = bpf_get_current_pid_tgid();
    let src_pid = (pid_tgid >> 32) as u32;
    let src_tid = pid_tgid as u32;

    let reply = unsafe { ctx.read_at::<i32>(OFF_REPLY) }? as u32;
    // Normal (successful) replies are noise — a code:0 ack per call. Drop them here,
    // before the ring buffer, still clearing the per-thread stash. Error replies come
    // from the binder_return attach point (M5). `--include-replies` re-enables them.
    if reply != 0 && INCLUDE_REPLIES.get(0).copied().unwrap_or(0) == 0 {
        let _ = STASH.remove(&pid_tgid);
        return Ok(());
    }

    let dst_pid = unsafe { ctx.read_at::<i32>(OFF_TO_PROC) }? as u32;
    let code = unsafe { ctx.read_at::<u32>(OFF_CODE) }?;
    let flags = unsafe { ctx.read_at::<u32>(OFF_FLAGS) }?;
    let debug_id = unsafe { ctx.read_at::<i32>(OFF_DEBUG_ID) }?;

    let mut ev = TxEvent {
        ts_ns: unsafe { bpf_ktime_get_ns() },
        src_pid,
        src_tid,
        dst_pid,
        code,
        flags,
        reply,
        data_size: 0,
        err_code: 0,
        debug_id,
        iface_byte_len: 0,
        iface: [0u8; MAX_IFACE_BYTES],
    };
    // Default keep when there's no stash (kprobe didn't run/insert): drop iff filtering
    // is active, since we then have no descriptor to match against a wanted interface.
    let mut keep = FILTER_ON.get(0).copied().unwrap_or(0) == 0;
    // Parcel buffer pointer (M6), carried from the kprobe; 0 when unavailable.
    let mut buf_ptr = 0u64;
    if let Some(stash) = unsafe { STASH.get(&pid_tgid) } {
        ev.data_size = stash.data_size;
        ev.iface_byte_len = stash.iface_byte_len;
        ev.iface = stash.iface;
        keep = stash.keep != 0;
        buf_ptr = stash.buf_ptr;
    }
    let _ = STASH.remove(&pid_tgid);

    if !keep {
        return Ok(());
    }

    // Error capture (M5): remember this kept, outgoing transaction so the binder_return
    // path can name the source → target and method if it fails. Only while error capture
    // is on (keeps the 256-byte descriptor copy off the hot path otherwise), and only for
    // real transactions — a reply (only here under --include-replies) provokes no error.
    // Insert `ev` by reference so no extra copy lands on the BPF stack.
    if reply == 0 && ERRORS_ON.get(0).copied().unwrap_or(0) != 0 {
        let _ = LAST_TX.insert(&pid_tgid, &ev, 0);
    }

    // Parcel payload capture (M6): only for real transactions we have a buffer pointer
    // for, and only while enabled. Emits a variable-length TxRecord (header + raw bytes)
    // instead of a bare TxEvent; the copy is staged in a per-CPU map, never the BPF stack.
    if reply == 0 && buf_ptr != 0 && PARCEL_ON.get(0).copied().unwrap_or(0) != 0 {
        let cap = PARCEL_MAX.get(0).copied().unwrap_or(0) as usize;
        if cap > 0 {
            emit_with_parcel(&ev, buf_ptr, ev.data_size, cap);
            return Ok(());
        }
    }

    if let Some(mut entry) = EVENTS.reserve::<TxEvent>(0) {
        entry.write(ev);
        entry.submit(0);
    }
    Ok(())
}

/// Stage a [`TxRecord`] in the per-CPU scratch map and emit it to the ring buffer as a
/// **variable-length** record: `ev` + `parcel_len` + exactly `parcel_len` payload bytes
/// read from the sender's userspace at `buf_ptr` (parcel offset 0), where `parcel_len =
/// min(data_size, cap, PARCEL_CEILING)`. Only the captured bytes hit the ring, so a big
/// cap costs ring space only when a big parcel actually flows. Nothing large touches the
/// 512-byte BPF stack.
fn emit_with_parcel(ev: &TxEvent, buf_ptr: u64, data_size: u32, cap: usize) {
    let Some(rec) = PARCEL_SCRATCH.get_ptr_mut(0) else {
        return;
    };
    // Length actually captured; a local (not re-read from map memory) so the verifier
    // keeps the `<= PARCEL_CEILING` bound when it sizes the ring write below.
    let mut plen = 0usize;
    unsafe {
        (*rec).ev = *ev;
        (*rec).parcel_len = 0;
        // Clamp the runtime cap against the constant ceiling last, so the read length is
        // bounded by a constant (a reg-vs-reg min alone doesn't propagate the bound).
        let n = core::cmp::min(core::cmp::min(data_size as usize, cap), PARCEL_CEILING);
        if n > 0 {
            // Raw pointer to the payload array — avoid an autoref into the map value.
            let parcel_ptr = core::ptr::addr_of_mut!((*rec).parcel) as *mut u8;
            let dst = core::slice::from_raw_parts_mut(parcel_ptr, n);
            if bpf_probe_read_user_buf(buf_ptr as *const u8, dst).is_ok() {
                (*rec).parcel_len = n as u32;
                plen = n;
            }
        }
        // Emit only the header + captured bytes. `plen <= PARCEL_CEILING`, so the total is
        // within the scratch value the verifier knows the bounds of.
        let out_len = core::mem::offset_of!(TxRecord, parcel) + plen;
        let bytes = core::slice::from_raw_parts(rec as *const u8, out_len);
        let _ = EVENTS.output(bytes, 0);
    }
}

/// Return/error path (M5). Fires for every binder return command written back to a
/// thread; we act only on the transaction-failure ones and only while error capture is
/// enabled. The failing thread is the current one, and its last captured transaction is
/// in [`LAST_TX`], so we can name the source → target and method that failed.
#[tracepoint(category = "binder", name = "binder_return")]
pub fn binder_return(ctx: TracePointContext) -> u32 {
    match try_return(&ctx) {
        Ok(()) => 0,
        Err(_) => 1,
    }
}

fn try_return(ctx: &TracePointContext) -> Result<(), i64> {
    if ERRORS_ON.get(0).copied().unwrap_or(0) == 0 {
        return Ok(());
    }
    let cmd = unsafe { ctx.read_at::<u32>(OFF_RETURN_CMD) }?;
    if !is_error_return(cmd) {
        return Ok(());
    }

    let pid_tgid = bpf_get_current_pid_tgid();
    let mut ev = TxEvent {
        ts_ns: unsafe { bpf_ktime_get_ns() },
        src_pid: (pid_tgid >> 32) as u32,
        src_tid: pid_tgid as u32,
        dst_pid: 0,
        code: 0,
        flags: 0,
        reply: 0,
        data_size: 0,
        err_code: cmd,
        debug_id: 0,
        iface_byte_len: 0,
        iface: [0u8; MAX_IFACE_BYTES],
    };
    // Correlate to this thread's last captured transaction. No record → we weren't
    // capturing that transaction (e.g. filtered out), so don't report an orphan error.
    // Read the fields through the map reference (a single 256-byte descriptor copy into
    // `ev`); materializing a whole `LastTx` on the stack alongside `ev` would blow the
    // 512-byte BPF stack.
    match unsafe { LAST_TX.get(&pid_tgid) } {
        Some(last) => {
            ev.dst_pid = last.dst_pid;
            ev.code = last.code;
            ev.debug_id = last.debug_id;
            ev.iface_byte_len = last.iface_byte_len;
            ev.iface = last.iface;
        }
        None => return Ok(()),
    }
    // One failure per transaction: drop the record so a later unrelated return on this
    // thread can't re-attribute the same call.
    let _ = LAST_TX.remove(&pid_tgid);

    if let Some(mut entry) = EVENTS.reserve::<TxEvent>(0) {
        entry.write(ev);
        entry.submit(0);
    }
    Ok(())
}

#[cfg(not(test))]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}
