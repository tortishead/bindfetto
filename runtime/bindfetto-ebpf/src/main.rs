#![no_std]
#![no_main]

//! Bindfetto probe (M1–M3).
//!
//! Two attach points, correlated per-thread:
//!
//! * **kprobe on `binder_transaction()`** — runs at function entry, reads the
//!   parcel size and the interface descriptor from the `binder_transaction_data`
//!   argument, and stashes them keyed by pid_tgid.
//! * **tracepoint `binder:binder_transaction`** — runs later inside the same call,
//!   reads the target pid / code / flags, pulls the stashed size+descriptor, and
//!   emits a [`TxEvent`] to the ring buffer.

use aya_ebpf::{
    helpers::{
        bpf_get_current_pid_tgid, bpf_ktime_get_ns, bpf_probe_read_kernel, bpf_probe_read_user,
        bpf_probe_read_user_buf,
    },
    macros::{kprobe, map, tracepoint},
    maps::{HashMap, RingBuf},
    programs::{ProbeContext, TracePointContext},
};
use bindfetto_common::{TxEvent, IFACE_HEADER_MAGIC, MAX_IFACE_BYTES};

/// Ring buffer to userspace.
#[map]
static EVENTS: RingBuf = RingBuf::with_byte_size(256 * 1024, 0);

/// Per-thread hand-off from the kprobe to the tracepoint, keyed by pid_tgid.
#[map]
static STASH: HashMap<u64, Stash> = HashMap::with_max_entries(10240, 0);

#[repr(C)]
#[derive(Clone, Copy)]
struct Stash {
    data_size: u32,
    iface_byte_len: u32,
    iface: [u8; MAX_IFACE_BYTES],
}

// --- binder:binder_transaction tracepoint field offsets (from the format file) ---
const OFF_TO_PROC: usize = 16;
const OFF_REPLY: usize = 24;
const OFF_CODE: usize = 28;
const OFF_FLAGS: usize = 32;

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
    // arg2 = struct binder_transaction_data *tr (kernel pointer to a copied-in struct)
    let tr: usize = ctx.arg(2).ok_or(1i64)?;

    let data_size =
        unsafe { bpf_probe_read_kernel((tr + TR_DATA_SIZE) as *const u64) }? as u32;
    let buf_ptr = unsafe { bpf_probe_read_kernel((tr + TR_BUFFER_PTR) as *const u64) }? as usize;

    let mut stash = Stash {
        data_size,
        iface_byte_len: 0,
        iface: [0u8; MAX_IFACE_BYTES],
    };

    // The parcel data lives in the sender's userspace at buf_ptr. If it starts with
    // an interface token, offset 8 holds the 'SYST' magic.
    let magic = unsafe { bpf_probe_read_user((buf_ptr + P_MAGIC) as *const u32) }.unwrap_or(0);
    if magic == IFACE_HEADER_MAGIC {
        let units = unsafe { bpf_probe_read_user((buf_ptr + P_STRLEN) as *const u32) }.unwrap_or(0);
        let nbytes = core::cmp::min((units as usize).saturating_mul(2), MAX_IFACE_BYTES);
        // Read a fixed MAX to keep the verifier happy; the logical length is nbytes.
        if unsafe { bpf_probe_read_user_buf((buf_ptr + P_STR) as *const u8, &mut stash.iface) }
            .is_ok()
        {
            stash.iface_byte_len = nbytes as u32;
        }
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
    // from a separate attach point (M5). An --include-replies flag can re-enable them.
    if reply != 0 {
        let _ = STASH.remove(&pid_tgid);
        return Ok(());
    }

    let dst_pid = unsafe { ctx.read_at::<i32>(OFF_TO_PROC) }? as u32;
    let code = unsafe { ctx.read_at::<u32>(OFF_CODE) }?;
    let flags = unsafe { ctx.read_at::<u32>(OFF_FLAGS) }?;

    let mut ev = TxEvent {
        ts_ns: unsafe { bpf_ktime_get_ns() },
        src_pid,
        src_tid,
        dst_pid,
        code,
        flags,
        reply,
        data_size: 0,
        iface_byte_len: 0,
        iface: [0u8; MAX_IFACE_BYTES],
    };
    if let Some(stash) = unsafe { STASH.get(&pid_tgid) } {
        ev.data_size = stash.data_size;
        ev.iface_byte_len = stash.iface_byte_len;
        ev.iface = stash.iface;
    }
    let _ = STASH.remove(&pid_tgid);

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
