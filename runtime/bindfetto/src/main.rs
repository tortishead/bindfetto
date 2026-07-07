//! Bindfetto userspace consumer (M1–M3).
//!
//! Loads the probe, attaches the kprobe + tracepoint, drains the ring buffer, and
//! prints one line per transaction:
//!
//!   <name> (<pid>) -> <name> (<pid>): <interface>.[code:N], <size>B [oneway]
//!
//! Process names come from `/proc/<pid>/cmdline` (cached). The interface
//! descriptor is decoded from the UTF-16LE bytes captured by the probe; the method
//! name itself is resolved offline against the AIDL catalog (later milestone).

use std::collections::HashMap;
use std::fs;

use anyhow::Context as _;
use aya::{
    maps::RingBuf,
    programs::{KProbe, TracePoint},
    Ebpf,
};
use bindfetto_common::TxEvent;
use tokio::io::unix::AsyncFd;

// eBPF object built by build.rs (aya-build).
static EBPF_OBJ: &[u8] = aya::include_bytes_aligned!(concat!(env!("OUT_DIR"), "/bindfetto"));

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut ebpf = Ebpf::load(EBPF_OBJ).context("load eBPF object")?;

    let kp: &mut KProbe = ebpf
        .program_mut("binder_transaction_enter")
        .context("program `binder_transaction_enter` missing")?
        .try_into()?;
    kp.load()?;
    kp.attach("binder_transaction", 0)
        .context("attach kprobe binder_transaction")?;

    let tp: &mut TracePoint = ebpf
        .program_mut("binder_transaction")
        .context("program `binder_transaction` missing")?
        .try_into()?;
    tp.load()?;
    tp.attach("binder", "binder_transaction")
        .context("attach binder:binder_transaction (need root + BPF-permissive SELinux)")?;

    let ring = RingBuf::try_from(ebpf.take_map("EVENTS").context("EVENTS map missing")?)?;
    let mut async_ring = AsyncFd::new(ring)?;
    let mut names = NameCache::default();
    // Kernel events carry CLOCK_MONOTONIC ns; this offset maps them to wall-clock.
    let boot_offset_ns = monotonic_to_realtime_offset_ns();

    println!("bindfetto: capturing binder transactions (Ctrl-C to stop)");

    loop {
        let mut guard = async_ring.readable_mut().await?;
        let ring = guard.get_inner_mut();
        while let Some(item) = ring.next() {
            let ev: &TxEvent = unsafe { &*(item.as_ptr() as *const TxEvent) };
            print_event(ev, &mut names, boot_offset_ns);
        }
        guard.clear_ready();
    }
}

fn print_event(ev: &TxEvent, names: &mut NameCache, boot_offset_ns: i128) {
    let ts = format_timestamp(ev.ts_ns, boot_offset_ns);
    let src = names.get(ev.src_pid).to_owned();
    let dst = names.get(ev.dst_pid).to_owned();
    let oneway = if ev.is_oneway() { " oneway" } else { "" };
    // When there's no AIDL interface token: a reply carries none by design; anything
    // else is likely HIDL/hwbinder or a special transaction, not an AIDL call.
    let label = match decode_iface(ev) {
        Some(iface) => format!("{iface}.[code:{}]", ev.code),
        None if ev.reply != 0 => format!("<reply code:{}>", ev.code),
        None => format!("<non-aidl code:{}>", ev.code),
    };
    println!(
        "{ts} {src} ({}) -> {dst} ({}): {label}, {}B{oneway}",
        ev.src_pid, ev.dst_pid, ev.data_size
    );
}

/// Nanoseconds to add to a `CLOCK_MONOTONIC` timestamp to get `CLOCK_REALTIME`
/// (Unix epoch) nanoseconds. Sampled once; good enough for display.
fn monotonic_to_realtime_offset_ns() -> i128 {
    fn clock_ns(clk: libc::clockid_t) -> i128 {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        unsafe { libc::clock_gettime(clk, &mut ts) };
        ts.tv_sec as i128 * 1_000_000_000 + ts.tv_nsec as i128
    }
    clock_ns(libc::CLOCK_REALTIME) - clock_ns(libc::CLOCK_MONOTONIC)
}

/// Format a kernel monotonic timestamp as local wall-clock `HH:MM:SS.mmm`.
fn format_timestamp(ts_ns: u64, boot_offset_ns: i128) -> String {
    let wall_ns = ts_ns as i128 + boot_offset_ns;
    let secs = (wall_ns / 1_000_000_000) as i64;
    let nsec = (wall_ns % 1_000_000_000) as u32;
    match chrono::DateTime::from_timestamp(secs, nsec) {
        Some(dt) => dt
            .with_timezone(&chrono::Local)
            .format("%H:%M:%S%.3f")
            .to_string(),
        None => "--:--:--.---".to_string(),
    }
}

/// Decode the interface descriptor from the event's UTF-16LE bytes.
fn decode_iface(ev: &TxEvent) -> Option<String> {
    let len = ev.iface_byte_len as usize;
    if len == 0 || len > ev.iface.len() {
        return None;
    }
    let units: Vec<u16> = ev.iface[..len]
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    let s = String::from_utf16_lossy(&units);
    let s = s.trim_end_matches('\0');
    (!s.is_empty()).then(|| s.to_owned())
}

/// pid -> process name, cached (a pid's name is stable for its lifetime).
#[derive(Default)]
struct NameCache(HashMap<u32, String>);

impl NameCache {
    fn get(&mut self, pid: u32) -> &str {
        self.0.entry(pid).or_insert_with(|| resolve_name(pid))
    }
}

fn resolve_name(pid: u32) -> String {
    // /proc/<pid>/cmdline: NUL-separated argv; the first field is the process name.
    if let Ok(bytes) = fs::read(format!("/proc/{pid}/cmdline")) {
        let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
        if end > 0 {
            return String::from_utf8_lossy(&bytes[..end]).into_owned();
        }
    }
    // Fallback: /proc/<pid>/comm (truncated to 15 chars by the kernel).
    if let Ok(s) = fs::read_to_string(format!("/proc/{pid}/comm")) {
        let t = s.trim_end();
        if !t.is_empty() {
            return t.to_owned();
        }
    }
    format!("pid:{pid}")
}
