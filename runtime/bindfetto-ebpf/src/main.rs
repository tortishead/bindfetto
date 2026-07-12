#![no_std]
#![no_main]

//! Bindfetto probe (M1–M6).
//!
//! Three attach points, correlated per-thread:
//!
//! * **kprobe on `binder_transaction()`** — runs at function entry, reads the
//!   parcel size and the interface descriptor from the `binder_transaction_data`
//!   argument, applies the interface filter, and stashes them keyed by pid_tgid
//!   (a filtered-out transaction inserts nothing — absence is the drop signal).
//! * **tracepoint `binder:binder_transaction`** — runs later inside the same call,
//!   reads the target pid / code / flags, pulls the stashed size+descriptor, and
//!   emits a [`TxEvent`] — or, with parcel capture on (M6), a variable-length
//!   [`TxRecord`] staged in a per-CPU scratch map — to the ring buffer.
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
    EbpfContext as _,
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
/// output volume and observer effect on the traced device. Using the full descriptor as
/// the key makes the match collision-free and exact (the kernel htab still jhashes the
/// 256-byte key internally; what we avoid is any truncation/collision handling of ours).
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

/// Per-thread hand-off payload from the kprobe to the tracepoint. **Presence is the
/// keep signal**: when the interface filter drops a transaction, the kprobe skips the
/// insert entirely (the dominant case while filtering), so the dropped path never pays
/// the ~272-byte map copy and the tracepoint never pays the read-back. Layout is
/// gap-free (u64 then two u32s, then the array) — the kprobe copies the whole struct
/// into the map and the verifier rejects reads of uninitialized padding.
#[repr(C)]
#[derive(Clone, Copy)]
struct Stash {
    /// Sender-side (untagged) userspace pointer to the parcel buffer, handed to the
    /// tracepoint so it can copy the raw payload straight into the ring slot (M6) — the
    /// bytes never touch the BPF stack. 0 when the kprobe couldn't read it.
    buf_ptr: u64,
    data_size: u32,
    iface_byte_len: u32,
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
    // upper bits for int. Only replies pay the INCLUDE_REPLIES map lookup.
    let reply: usize = ctx.arg(3).ok_or(1i64)?;
    let is_reply = reply as u32 != 0;
    if is_reply && INCLUDE_REPLIES.get(0).copied().unwrap_or(0) == 0 {
        return Ok(());
    }

    // arg2 = struct binder_transaction_data *tr (kernel pointer to a copied-in struct).
    // data_size (+32), offsets_size (+40, unused) and data.ptr.buffer (+48) are
    // contiguous, so one 24-byte read replaces two helper calls.
    let tr: usize = ctx.arg(2).ok_or(1i64)?;
    let tr_words = unsafe { bpf_probe_read_kernel((tr + TR_DATA_SIZE) as *const [u64; 3]) }?;
    let data_size = tr_words[0] as u32;
    // data.ptr.buffer is a sender-side userspace pointer. On AArch64 it may carry a tag
    // in the top byte (TBI is always on; ARM MTE puts a logical tag in bits 59-56).
    // Strip the top byte before pointer arithmetic + user reads so we don't depend on a
    // given kernel untagging it for us; user VAs live in TTBR0 low range, so the top
    // byte is purely tag and this is a no-op on untagged pointers.
    let buf_ptr = (tr_words[2] as usize) & 0x00ff_ffff_ffff_ffff;

    let mut stash = Stash {
        buf_ptr: buf_ptr as u64,
        data_size,
        iface_byte_len: 0,
        iface: [0u8; MAX_IFACE_BYTES],
    };

    // The parcel data lives in the sender's userspace at buf_ptr. If it starts with
    // an interface token, offset 8 holds the 'SYST' magic and offset 12 the UTF-16
    // code-unit count — adjacent, so one u64 read fetches both (LE: magic in the low
    // half). A parcel smaller than the token header (16 bytes) can't carry one — don't
    // even read the magic. Replies (only reachable here under `--include-replies`)
    // never carry a token, so skip it.
    if !is_reply && data_size as usize >= P_STR {
        let head = unsafe { bpf_probe_read_user((buf_ptr + P_MAGIC) as *const u64) }.unwrap_or(0);
        if head as u32 == IFACE_HEADER_MAGIC {
            let units = (head >> 32) as u32;
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

    let pid_tgid = bpf_get_current_pid_tgid();

    // Interface filter (M4): when enabled, keep only transactions whose captured
    // descriptor is in WANTED (full descriptor as the key = exact, collision-free
    // match). Tokenless / unreadable transactions have an all-zero key that userspace
    // never inserts, so they drop while filtering is on. A dropped transaction **skips
    // the stash insert entirely** — no stash is the tracepoint's drop signal — so the
    // filtered-out majority never pays the ~272-byte map copy. Remove any stale entry
    // instead (a prior call on this thread that failed before its tracepoint would
    // otherwise leave one behind to be mis-attributed).
    if FILTER_ON.get(0).copied().unwrap_or(0) != 0 {
        // Reinterpret the captured bytes as the key in place — IfaceKey is
        // repr(transparent) over [u8; MAX_IFACE_BYTES], so this avoids a second
        // 256-byte copy that would blow the 512-byte BPF stack.
        let key = unsafe { &*(stash.iface.as_ptr() as *const IfaceKey) };
        if unsafe { WANTED.get(key) }.is_none() {
            let _ = STASH.remove(&pid_tgid);
            return Ok(());
        }
    }

    let _ = STASH.insert(&pid_tgid, &stash, 0);
    Ok(())
}

/// Read a tracepoint field directly from the context at a constant offset. For
/// tracepoint programs the verifier allows direct (aligned, in-bounds) ctx loads, so
/// this compiles to a single load instruction — aya's `read_at` goes through the
/// `bpf_probe_read` helper instead, one call per field.
#[inline(always)]
fn tp_read<T: Copy>(ctx: &TracePointContext, off: usize) -> T {
    unsafe { core::ptr::read((ctx.as_ptr() as *const u8).add(off) as *const T) }
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

    let reply = tp_read::<i32>(ctx, OFF_REPLY) as u32;
    // Normal (successful) replies are noise — a code:0 ack per call. Drop them here,
    // before the ring buffer, still clearing the per-thread stash. Error replies come
    // from the binder_return attach point (M5). `--include-replies` re-enables them.
    if reply != 0 && INCLUDE_REPLIES.get(0).copied().unwrap_or(0) == 0 {
        let _ = STASH.remove(&pid_tgid);
        return Ok(());
    }

    // Consult the stash *before* building anything: no stash while filtering means the
    // kprobe dropped this transaction (or couldn't capture a descriptor to match), so
    // the filtered-out majority exits here having paid only a hash lookup — no field
    // reads, no ktime, no 304-byte event init.
    let stash = unsafe { STASH.get(&pid_tgid) };
    if stash.is_none() && FILTER_ON.get(0).copied().unwrap_or(0) != 0 {
        return Ok(());
    }

    let mut ev = TxEvent {
        ts_ns: unsafe { bpf_ktime_get_ns() },
        src_pid: (pid_tgid >> 32) as u32,
        src_tid: pid_tgid as u32,
        dst_pid: tp_read::<i32>(ctx, OFF_TO_PROC) as u32,
        code: tp_read::<u32>(ctx, OFF_CODE),
        flags: tp_read::<u32>(ctx, OFF_FLAGS),
        reply,
        data_size: 0,
        err_code: 0,
        debug_id: tp_read::<i32>(ctx, OFF_DEBUG_ID),
        iface_byte_len: 0,
        iface: [0u8; MAX_IFACE_BYTES],
    };
    // Parcel buffer pointer (M6), carried from the kprobe; 0 when unavailable.
    let mut buf_ptr = 0u64;
    // Copy out of the map value *before* the remove — the element can be reused by
    // another CPU's insert the moment it's deleted.
    let had_stash = if let Some(stash) = stash {
        ev.data_size = stash.data_size;
        ev.iface_byte_len = stash.iface_byte_len;
        ev.iface = stash.iface;
        buf_ptr = stash.buf_ptr;
        true
    } else {
        false
    };
    if had_stash {
        let _ = STASH.remove(&pid_tgid);
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
    let cmd = tp_read::<u32>(ctx, OFF_RETURN_CMD);
    if !is_error_return(cmd) {
        return Ok(());
    }

    // Correlate to this thread's last captured transaction *before* building the event.
    // No record → we weren't capturing that transaction (e.g. filtered out), so don't
    // report an orphan error — and don't pay the 304-byte event init for it.
    let pid_tgid = bpf_get_current_pid_tgid();
    let Some(last) = (unsafe { LAST_TX.get(&pid_tgid) }) else {
        return Ok(());
    };

    // Read the fields through the map reference (a single 256-byte descriptor copy into
    // `ev`); materializing a whole copy on the stack alongside `ev` would blow the
    // 512-byte BPF stack. Copy before the remove — the element can be reused by another
    // CPU's insert the moment it's deleted.
    let ev = TxEvent {
        ts_ns: unsafe { bpf_ktime_get_ns() },
        src_pid: (pid_tgid >> 32) as u32,
        src_tid: pid_tgid as u32,
        dst_pid: last.dst_pid,
        code: last.code,
        flags: 0,
        reply: 0,
        data_size: 0,
        err_code: cmd,
        debug_id: last.debug_id,
        iface_byte_len: last.iface_byte_len,
        iface: last.iface,
    };
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
