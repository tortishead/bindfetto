//! Bindfetto userspace consumer (M1–M5).
//!
//! Loads the probe, attaches the kprobe + the two tracepoints (`binder_transaction`
//! plus the `binder_return` error path), drains the ring buffer, and emits one line per
//! transaction to the selected sink:
//!
//!   <name> (<pid>) -> <name> (<pid>): <interface>.[code:N], <size>B [oneway]
//!
//! Process names come from `/proc/<pid>/cmdline` (cached). The interface
//! descriptor is decoded from the UTF-16LE bytes captured by the probe; the method
//! name itself is resolved offline against the AIDL catalog (later milestone).
//!
//! Sinks: `--sink console|logcat|both|none` (default console) for human-readable
//! lines — logcat lines use tag `bindfetto` and carry the `BINDFETTO` marker so the
//! offline decoder can select them. Independently:
//!
//! * `--jsonl <path>` writes one structured JSON object per transaction to a file for
//!   offline capture and decoding.
//! * `--dlt-serve [port]` makes bindfetto itself a DLT server (default port 3490):
//!   DLT Viewer connects over TCP and shows the transactions **live**, even without an
//!   OEM logcat->DLT bridge or any dlt-daemon.
//!
//! Both compose with any `--sink` (use `--sink none` for a quiet, sink-only capture).
//!
//! Error capture (M5) is a separate, toggleable attach point (`--errors [on|off]`, off
//! by default; also toggled live over the control channel): it reports
//! `BR_FAILED_REPLY`/`BR_DEAD_REPLY`/`BR_FROZEN_REPLY` correlated to the failing
//! source → target, and — matched by transaction `debug_id` against the kernel's binder
//! `failed_transaction_log` — the *concrete* failure errno (e.g. `-ENOSPC` = the target's
//! binder buffer is full). `--include-replies` additionally keeps normal replies.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::io::{BufWriter, Write as _};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Context as _;
use aya::{
    maps::{Array, HashMap as BpfHashMap, MapData, RingBuf},
    programs::{KProbe, TracePoint},
    Ebpf,
};
use bindfetto_common::{IfaceKey, TxEvent, TxRecord, PARCEL_CAP_DEFAULT, PARCEL_CEILING};
// `dlt as dlt_wire`: the consumer's own `mod dlt` is the TCP server; the core module is
// the wire encoder it feeds.
use bindfetto_core::{
    br_error_name, comm_name, dlt as dlt_wire, errno_reason, iface_key, json_escape, push_hex,
    write_iface_bytes,
};
use tokio::io::unix::AsyncFd;

// eBPF object built by build.rs (aya-build).
static EBPF_OBJ: &[u8] = aya::include_bytes_aligned!(concat!(env!("OUT_DIR"), "/bindfetto"));

/// Logcat tag; the decoder can select bindfetto lines with `logcat -s bindfetto`.
const LOG_TAG: &str = "bindfetto";
/// In-message marker so bindfetto lines are identifiable even in a merged/DLT log
/// where the tag may be flattened.
const LOG_MARKER: &str = "BINDFETTO";

/// Destination for the human-readable transaction lines.
#[derive(Clone, Copy)]
enum Sink {
    Console,
    Logcat,
    Both,
    /// Neither text sink — for a quiet, file-only (`--jsonl`) capture.
    None,
}

impl Sink {
    fn console(self) -> bool {
        matches!(self, Sink::Console | Sink::Both)
    }
    fn logcat(self) -> bool {
        matches!(self, Sink::Logcat | Sink::Both)
    }

    fn parse(args: &[String]) -> Self {
        Sink::from_name(arg_value(args, "--sink").unwrap_or("console")).unwrap_or(Sink::Console)
    }

    /// Parse a sink name (`console|logcat|both|none`).
    fn from_name(name: &str) -> Option<Self> {
        match name {
            "console" => Some(Sink::Console),
            "logcat" => Some(Sink::Logcat),
            "both" => Some(Sink::Both),
            "none" => Some(Sink::None),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Sink::Console => "console",
            Sink::Logcat => "logcat",
            Sink::Both => "both",
            Sink::None => "none",
        }
    }

    /// Stable encoding for the lock-free `AtomicU8` in [`RuntimeState`].
    fn as_u8(self) -> u8 {
        match self {
            Sink::Console => 0,
            Sink::Logcat => 1,
            Sink::Both => 2,
            Sink::None => 3,
        }
    }

    fn from_u8(v: u8) -> Self {
        match v {
            1 => Sink::Logcat,
            2 => Sink::Both,
            3 => Sink::None,
            _ => Sink::Console,
        }
    }
}

/// The value following `flag` in the args, if present (`--flag value`).
fn arg_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

/// Parse a boolean flag that may be bare or take an explicit `on`/`off` value
/// (`--errors`, `--errors on`, `--errors off`). Absent → false; bare or a truthy value
/// → true; `off`/`false`/`0` → false.
fn flag_on_off(args: &[String], flag: &str) -> bool {
    match args.iter().position(|a| a == flag) {
        None => false,
        Some(i) => !matches!(args.get(i + 1).map(String::as_str), Some("off" | "false" | "0")),
    }
}

/// All interface names requested via `--iface`. Repeatable and comma-separated
/// (`--iface a.b.IFoo --iface a.c.IBar,a.c.IBaz`); blanks are ignored.
fn iface_filters(args: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--iface" {
            if let Some(v) = args.get(i + 1) {
                out.extend(
                    v.split(',')
                        .map(str::trim)
                        .filter(|p| !p.is_empty())
                        .map(str::to_owned),
                );
            }
            i += 2;
        } else {
            i += 1;
        }
    }
    out
}

/// Minimal binding to Android's liblog for the logcat sink.
mod logcat {
    use std::ffi::CString;
    use std::os::raw::{c_char, c_int};

    const ANDROID_LOG_INFO: c_int = 4;

    #[link(name = "log")]
    extern "C" {
        fn __android_log_write(prio: c_int, tag: *const c_char, text: *const c_char) -> c_int;
    }

    pub fn write(tag: &str, msg: &str) {
        if let (Ok(tag), Ok(msg)) = (CString::new(tag), CString::new(msg)) {
            unsafe { __android_log_write(ANDROID_LOG_INFO, tag.as_ptr(), msg.as_ptr()) };
        }
    }
}

/// Embedded DLT server: bindfetto itself is the endpoint DLT Viewer connects to.
///
/// It listens on TCP and streams each transaction as a verbose DLT message (see
/// [`dlt_wire`]). This makes bindfetto self-contained for DLT Viewer **live trace**
/// with no libdlt and no separate `dlt-daemon` — the fallback for targets where the
/// OEM does not bridge logcat into DLT. Connect DLT Viewer to the port as a TCP ECU
/// (e.g. `adb forward tcp:3490 tcp:3490`, then `localhost:3490`).
mod dlt {
    use std::sync::Arc;

    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;
    use tokio::sync::broadcast;

    /// A running DLT TCP server. Encoded messages handed to [`send`](Self::send) are
    /// fanned out to every currently-connected DLT Viewer client.
    pub struct DltServer {
        tx: broadcast::Sender<Arc<[u8]>>,
    }

    impl DltServer {
        /// Bind the server and spawn its accept loop.
        pub async fn bind(port: u16) -> std::io::Result<Self> {
            let listener = TcpListener::bind(("0.0.0.0", port)).await?;
            // Buffer a burst of messages per client; a slow/absent client just lags.
            let (tx, _rx) = broadcast::channel::<Arc<[u8]>>(4096);
            let accept_tx = tx.clone();
            tokio::spawn(async move {
                while let Ok((sock, _addr)) = listener.accept().await {
                    let mut rx = accept_tx.subscribe();
                    tokio::spawn(async move {
                        let (_read, mut write) = sock.into_split();
                        loop {
                            match rx.recv().await {
                                Ok(buf) => {
                                    if write.write_all(&buf).await.is_err() {
                                        break; // client went away
                                    }
                                }
                                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                                Err(broadcast::error::RecvError::Closed) => break,
                            }
                        }
                    });
                }
            });
            Ok(DltServer { tx })
        }

        /// Fan an encoded message out to connected clients (non-blocking; a no-op when
        /// nobody is connected).
        pub fn send(&self, bytes: Arc<[u8]>) {
            let _ = self.tx.send(bytes);
        }
    }
}

/// Owns the two BPF filter maps and the current filter set, so the interface filter
/// can be reconfigured at runtime (from the control channel) as well as at startup
/// (`--iface`). Replacing the set removes the previous keys, inserts the new ones, and
/// flips `FILTER_ON` — the probe reads both maps directly on the hot path.
struct FilterCtl {
    wanted: BpfHashMap<MapData, IfaceKey, u8>,
    filter_on: Array<MapData, u32>,
    active: Vec<String>,
}

impl FilterCtl {
    /// Replace the wanted-interface set. Empty `names` disables filtering.
    fn apply(&mut self, names: &[String]) -> anyhow::Result<()> {
        for old in &self.active {
            let _ = self.wanted.remove(&iface_key(old));
        }
        self.active.clear();
        for name in names {
            self.wanted
                .insert(iface_key(name), 1u8, 0)
                .with_context(|| format!("insert interface filter {name}"))?;
            self.active.push(name.clone());
        }
        let on = u32::from(!self.active.is_empty());
        self.filter_on.set(0, on, 0).context("set FILTER_ON")?;
        Ok(())
    }
}

/// Shared, live-tunable runtime state: read by the emitter/drain loop on the hot path
/// and mutated by the control channel. The scalar toggles are lock-free atomics; only
/// the observed-interface set and the filter maps need a mutex.
struct RuntimeState {
    /// Master capture toggle (`START`/`STOP`): when off, drained events are dropped.
    capturing: AtomicBool,
    /// Interface discovery (`TRACK`): when off (the default) the emitter does not record
    /// observed interfaces, so there's no discovery overhead until the app asks for it.
    discovering: AtomicBool,
    /// Active text sink, encoded via [`Sink::as_u8`] (`SINK`).
    sink: AtomicU8,
    /// Whether transactions are streamed to the DLT server (`DLT on|off`). The server is
    /// bound once at startup; this only gates the fan-out.
    dlt_on: AtomicBool,
    /// Whether error capture is on (`ERRORS on|off`); mirrors the ERRORS_ON BPF flag map
    /// for cheap `STATUS` reads.
    errors_on: AtomicBool,
    /// Whether parcel payload capture is on (`PARCEL on|off`, M6); mirrors the PARCEL_ON
    /// BPF flag map. Only enableable while the interface filter is active.
    parcel_on: AtomicBool,
    /// Runtime cap (bytes) on captured parcel payload (`PARCEL max <n>`); mirrors the
    /// PARCEL_MAX BPF map. Clamped to [`PARCEL_CEILING`].
    parcel_max: AtomicU32,
    /// The DLT server's port (0 if no server was bound); reported by `STATUS`.
    dlt_port: u16,
    /// Total transactions drained from the ring buffer.
    captured: AtomicU64,
    /// Transactions that passed the capture gate (were emitted/processed).
    emitted: AtomicU64,
    /// Every interface descriptor seen while discovering; feeds `LIST`.
    observed: Mutex<BTreeSet<String>>,
    /// The in-kernel interface filter (`LIST`/`GET`/`SET`/`CLEAR`).
    filter: Mutex<FilterCtl>,
    /// The ERRORS_ON BPF flag map, so `ERRORS on|off` can toggle error capture live.
    errors: Mutex<Array<MapData, u32>>,
    /// The PARCEL_ON BPF flag map, so `PARCEL on|off` can toggle payload capture live.
    parcel: Mutex<Array<MapData, u32>>,
    /// The PARCEL_MAX BPF map (runtime payload cap in bytes), so `PARCEL max <n>` retunes
    /// it live.
    parcel_max_map: Mutex<Array<MapData, u32>>,
}

impl RuntimeState {
    /// Set the PARCEL_ON flag map and mirror it into the `parcel_on` atomic. Enabling is
    /// refused unless the interface filter is currently active (non-empty), so parcel
    /// capture is always bounded to the operator's selected interfaces. Returns an error
    /// string suitable for the control reply.
    fn set_parcel(&self, on: bool) -> Result<(), String> {
        if on && self.filter.lock().unwrap().active.is_empty() {
            return Err("PARCEL needs an active interface filter (SET one first)".into());
        }
        self.parcel
            .lock()
            .unwrap()
            .set(0, u32::from(on), 0)
            .map_err(|e| e.to_string())?;
        self.parcel_on.store(on, Ordering::Relaxed);
        Ok(())
    }

    /// Set the runtime parcel cap (bytes), clamped to [`PARCEL_CEILING`]. Returns the
    /// value actually applied so the caller can report clamping.
    fn set_parcel_max(&self, bytes: u32) -> Result<u32, String> {
        let clamped = bytes.min(PARCEL_CEILING as u32);
        self.parcel_max_map
            .lock()
            .unwrap()
            .set(0, clamped, 0)
            .map_err(|e| e.to_string())?;
        self.parcel_max.store(clamped, Ordering::Relaxed);
        Ok(clamped)
    }
}

/// Control channel: a line-oriented TCP server the control app connects to (via
/// `adb forward` in dev, or localhost on-device). Commands, one per line:
///
/// * `STATUS` -> `key=value` lines (capturing/discovering/sink/dlt/dlt_port/filter/
///   captured/emitted), then `END`.
/// * `START` / `STOP` -> toggle capture; reply `OK`.
/// * `SINK console|logcat|both|none` -> switch the text sink; reply `OK`/`ERR`.
/// * `DLT on|off` -> toggle DLT streaming; reply `OK`.
/// * `ERRORS on|off` -> toggle error capture (BR_FAILED/DEAD_REPLY); reply `OK`.
/// * `PARCEL on|off` -> toggle parcel payload capture (needs an active filter); `OK`/`ERR`.
/// * `PARCEL max <bytes>` -> set the runtime payload cap (clamped to the ceiling); `OK <n>`.
/// * `TRACK on|off` -> toggle interface discovery; reply `OK`.
/// * `LIST` -> every interface descriptor seen so far, one per line, then `END`.
/// * `GET`  -> the interfaces in the active filter, one per line, then `END`.
/// * `SET a,b,c` -> replace the in-kernel filter; reply `OK <n>`.
/// * `CLEAR` -> disable filtering; reply `OK 0`.
mod control {
    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::{TcpListener, TcpStream};

    use super::{RuntimeState, Sink};

    /// Bind the control port and spawn the accept loop.
    pub async fn serve(port: u16, state: Arc<RuntimeState>) -> std::io::Result<()> {
        let listener = TcpListener::bind(("0.0.0.0", port)).await?;
        tokio::spawn(async move {
            while let Ok((sock, _addr)) = listener.accept().await {
                let state = state.clone();
                tokio::spawn(async move {
                    let _ = handle(sock, state).await;
                });
            }
        });
        Ok(())
    }

    fn on_off(rest: &str) -> Option<bool> {
        match rest.to_ascii_lowercase().as_str() {
            "on" | "1" | "true" => Some(true),
            "off" | "0" | "false" => Some(false),
            _ => None,
        }
    }

    async fn handle(sock: TcpStream, state: Arc<RuntimeState>) -> std::io::Result<()> {
        let (read, mut write) = sock.into_split();
        let mut lines = BufReader::new(read).lines();
        while let Some(line) = lines.next_line().await? {
            let line = line.trim();
            let (cmd, rest) = match line.split_once(' ') {
                Some((c, r)) => (c, r.trim()),
                None => (line, ""),
            };
            match cmd.to_ascii_uppercase().as_str() {
                "STATUS" => {
                    let onoff = |b: bool| if b { "on" } else { "off" };
                    let sink = Sink::from_u8(state.sink.load(Ordering::Relaxed));
                    let filter_n = state.filter.lock().unwrap().active.len();
                    let body = format!(
                        "capturing={}\ndiscovering={}\nsink={}\ndlt={}\ndlt_port={}\n\
                         errors={}\nparcel={}\nparcel_max={}\nfilter={}\ncaptured={}\n\
                         emitted={}\nEND\n",
                        onoff(state.capturing.load(Ordering::Relaxed)),
                        onoff(state.discovering.load(Ordering::Relaxed)),
                        sink.name(),
                        onoff(state.dlt_on.load(Ordering::Relaxed)),
                        state.dlt_port,
                        onoff(state.errors_on.load(Ordering::Relaxed)),
                        onoff(state.parcel_on.load(Ordering::Relaxed)),
                        state.parcel_max.load(Ordering::Relaxed),
                        filter_n,
                        state.captured.load(Ordering::Relaxed),
                        state.emitted.load(Ordering::Relaxed),
                    );
                    write.write_all(body.as_bytes()).await?;
                }
                "START" => {
                    state.capturing.store(true, Ordering::Relaxed);
                    write.write_all(b"OK\n").await?;
                }
                "STOP" => {
                    state.capturing.store(false, Ordering::Relaxed);
                    write.write_all(b"OK\n").await?;
                }
                "SINK" => match Sink::from_name(&rest.to_ascii_lowercase()) {
                    Some(s) => {
                        state.sink.store(s.as_u8(), Ordering::Relaxed);
                        write.write_all(b"OK\n").await?;
                    }
                    None => {
                        write
                            .write_all(b"ERR sink must be console|logcat|both|none\n")
                            .await?;
                    }
                },
                "DLT" => match on_off(rest) {
                    Some(on) => {
                        state.dlt_on.store(on, Ordering::Relaxed);
                        write.write_all(b"OK\n").await?;
                    }
                    None => write.write_all(b"ERR DLT needs on|off\n").await?,
                },
                "ERRORS" => match on_off(rest) {
                    Some(on) => {
                        // Flip the BPF flag map first; only reflect success in the atomic.
                        let res = state
                            .errors
                            .lock()
                            .unwrap()
                            .set(0, u32::from(on), 0);
                        match res {
                            Ok(()) => {
                                state.errors_on.store(on, Ordering::Relaxed);
                                write.write_all(b"OK\n").await?;
                            }
                            Err(e) => {
                                write.write_all(format!("ERR {e}\n").as_bytes()).await?;
                            }
                        }
                    }
                    None => write.write_all(b"ERR ERRORS needs on|off\n").await?,
                },
                "PARCEL" => {
                    // `PARCEL max <bytes>` retunes the cap; `PARCEL on|off` toggles capture.
                    let (sub, arg) = match rest.split_once(' ') {
                        Some((s, a)) => (s, a.trim()),
                        None => (rest, ""),
                    };
                    if sub.eq_ignore_ascii_case("max") {
                        match arg.parse::<u32>() {
                            Ok(bytes) => match state.set_parcel_max(bytes) {
                                Ok(applied) => {
                                    write.write_all(format!("OK {applied}\n").as_bytes()).await?
                                }
                                Err(e) => {
                                    write.write_all(format!("ERR {e}\n").as_bytes()).await?
                                }
                            },
                            Err(_) => write.write_all(b"ERR PARCEL max needs a byte count\n").await?,
                        }
                    } else {
                        match on_off(rest) {
                            Some(on) => match state.set_parcel(on) {
                                Ok(()) => write.write_all(b"OK\n").await?,
                                Err(e) => write.write_all(format!("ERR {e}\n").as_bytes()).await?,
                            },
                            None => write.write_all(b"ERR PARCEL needs on|off or max <bytes>\n").await?,
                        }
                    }
                }
                "TRACK" => match on_off(rest) {
                    Some(on) => {
                        state.discovering.store(on, Ordering::Relaxed);
                        write.write_all(b"OK\n").await?;
                    }
                    None => write.write_all(b"ERR TRACK needs on|off\n").await?,
                },
                "LIST" => {
                    let items: Vec<String> =
                        state.observed.lock().unwrap().iter().cloned().collect();
                    for it in items {
                        write.write_all(it.as_bytes()).await?;
                        write.write_all(b"\n").await?;
                    }
                    write.write_all(b"END\n").await?;
                }
                "GET" => {
                    let items = state.filter.lock().unwrap().active.clone();
                    for it in items {
                        write.write_all(it.as_bytes()).await?;
                        write.write_all(b"\n").await?;
                    }
                    write.write_all(b"END\n").await?;
                }
                "SET" => {
                    let names: Vec<String> = rest
                        .split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned)
                        .collect();
                    let n = names.len();
                    let res = state.filter.lock().unwrap().apply(&names);
                    match res {
                        Ok(()) => {
                            // Parcel capture requires an active filter; if this emptied it,
                            // drop parcel capture to keep the invariant.
                            if n == 0 {
                                let _ = state.set_parcel(false);
                            }
                            write.write_all(format!("OK {n}\n").as_bytes()).await?
                        }
                        Err(e) => write.write_all(format!("ERR {e}\n").as_bytes()).await?,
                    }
                }
                "CLEAR" => {
                    let res = state.filter.lock().unwrap().apply(&[]);
                    match res {
                        Ok(()) => {
                            // Clearing the filter disables parcel capture (it may not run
                            // device-wide).
                            let _ = state.set_parcel(false);
                            write.write_all(b"OK 0\n").await?
                        }
                        Err(e) => write.write_all(format!("ERR {e}\n").as_bytes()).await?,
                    }
                }
                "" => {}
                other => {
                    write
                        .write_all(format!("ERR unknown command {other}\n").as_bytes())
                        .await?;
                }
            }
        }
        Ok(())
    }
}

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

    // Second attach point (M5): the return/error path. Always attached; the probe itself
    // no-ops unless error capture is enabled via the ERRORS_ON map.
    let rp: &mut TracePoint = ebpf
        .program_mut("binder_return")
        .context("program `binder_return` missing")?
        .try_into()?;
    rp.load()?;
    rp.attach("binder", "binder_return")
        .context("attach binder:binder_return")?;

    let args: Vec<String> = std::env::args().collect();
    let sink = Sink::parse(&args);
    let jsonl = match arg_value(&args, "--jsonl") {
        Some(path) => Some(BufWriter::new(
            fs::File::create(path).with_context(|| format!("create jsonl file {path}"))?,
        )),
        None => None,
    };
    // The control channel drives the runtime live (Track C). Its presence changes a few
    // startup defaults: it auto-binds a DLT server (so the app's DLT toggle is real) and
    // enables the observed-interface set for discovery.
    let control_port = args.iter().position(|a| a == "--control").map(|i| {
        args.get(i + 1)
            .and_then(|s| s.parse::<u16>().ok())
            .unwrap_or(3491)
    });

    // Bind a DLT server if the user asked (`--dlt-serve`) or if the control channel is on
    // (so `DLT on` has a server to stream to). Explicit `--dlt-serve` also turns streaming
    // on immediately; under `--control` alone it stays off until the app enables it.
    let dlt_explicit = args.iter().position(|a| a == "--dlt-serve");
    let dlt_port = dlt_explicit
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(3490);
    let dlt = if dlt_explicit.is_some() || control_port.is_some() {
        let server = dlt::DltServer::bind(dlt_port)
            .await
            .with_context(|| format!("bind DLT server on port {dlt_port}"))?;
        println!("bindfetto: DLT server on 0.0.0.0:{dlt_port} — connect DLT Viewer as a TCP ECU");
        Some(DltState::new(server))
    } else {
        None
    };

    // In-kernel interface filter (M4): own the two BPF maps so the filter can be set at
    // startup (`--iface`) and reconfigured live over the control channel. Non-matching
    // transactions are dropped in the probe before they ever reach the ring buffer.
    let ifaces = iface_filters(&args);
    let mut filter = FilterCtl {
        wanted: BpfHashMap::try_from(ebpf.take_map("WANTED").context("WANTED map missing")?)?,
        filter_on: Array::try_from(
            ebpf.take_map("FILTER_ON").context("FILTER_ON map missing")?,
        )?,
        active: Vec::new(),
    };
    if !ifaces.is_empty() {
        filter.apply(&ifaces).context("apply startup --iface filter")?;
        println!(
            "bindfetto: in-kernel interface filter active — keeping {}: {}",
            ifaces.len(),
            ifaces.join(", ")
        );
    }

    // Error capture (M5): the ERRORS_ON flag map gates the binder_return attach point.
    // Off by default (per SPEC); `--errors [on|off]` sets the startup state and the
    // control channel can toggle it live, so keep the map handle in RuntimeState.
    let errors_enabled = flag_on_off(&args, "--errors");
    let mut errors_map: Array<MapData, u32> = Array::try_from(
        ebpf.take_map("ERRORS_ON").context("ERRORS_ON map missing")?,
    )?;
    errors_map
        .set(0, u32::from(errors_enabled), 0)
        .context("set ERRORS_ON")?;
    if errors_enabled {
        println!("bindfetto: error capture on — reporting BR_FAILED_REPLY/BR_DEAD_REPLY");
    }

    // `--include-replies`: keep normal (successful) replies instead of dropping them
    // before the ring buffer. Startup-only (a map so the probe reads it); set once here.
    let include_replies = args.iter().any(|a| a == "--include-replies");
    let mut include_map: Array<MapData, u32> = Array::try_from(
        ebpf.take_map("INCLUDE_REPLIES")
            .context("INCLUDE_REPLIES map missing")?,
    )?;
    include_map
        .set(0, u32::from(include_replies), 0)
        .context("set INCLUDE_REPLIES")?;
    if include_replies {
        println!("bindfetto: including replies in the capture");
    }

    // Parcel payload capture (M6): the PARCEL_ON flag map gates the in-probe copy of raw
    // parcel bytes. Only meaningful under an active interface filter — enabling it
    // device-wide would multiply ring traffic — so `--parcel on` is honored only when
    // `--iface` was also given; the control channel enforces the same rule live.
    let parcel_requested = flag_on_off(&args, "--parcel");
    let parcel_enabled = parcel_requested && !ifaces.is_empty();
    let mut parcel_map: Array<MapData, u32> = Array::try_from(
        ebpf.take_map("PARCEL_ON").context("PARCEL_ON map missing")?,
    )?;
    parcel_map
        .set(0, u32::from(parcel_enabled), 0)
        .context("set PARCEL_ON")?;
    // Runtime payload cap (`--parcel-max <bytes>`, default 256), clamped to the ceiling.
    let parcel_cap = arg_value(&args, "--parcel-max")
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(PARCEL_CAP_DEFAULT)
        .min(PARCEL_CEILING as u32);
    let mut parcel_max_map: Array<MapData, u32> = Array::try_from(
        ebpf.take_map("PARCEL_MAX").context("PARCEL_MAX map missing")?,
    )?;
    parcel_max_map
        .set(0, parcel_cap, 0)
        .context("set PARCEL_MAX")?;
    if parcel_requested && !parcel_enabled {
        println!(
            "bindfetto: --parcel ignored — parcel capture needs an interface filter \
             (pass --iface, or SET one over the control channel then PARCEL on)"
        );
    } else if parcel_enabled {
        println!("bindfetto: parcel payload capture on (up to {parcel_cap}B/transaction)");
    }

    // Shared runtime state: capture on by default, discovery off (only tracked when the
    // app asks), sink from `--sink`, DLT streaming on iff `--dlt-serve` was explicit.
    let state = Arc::new(RuntimeState {
        capturing: AtomicBool::new(true),
        discovering: AtomicBool::new(false),
        sink: AtomicU8::new(sink.as_u8()),
        dlt_on: AtomicBool::new(dlt_explicit.is_some()),
        errors_on: AtomicBool::new(errors_enabled),
        parcel_on: AtomicBool::new(parcel_enabled),
        parcel_max: AtomicU32::new(parcel_cap),
        dlt_port: if dlt.is_some() { dlt_port } else { 0 },
        captured: AtomicU64::new(0),
        emitted: AtomicU64::new(0),
        observed: Mutex::new(BTreeSet::new()),
        filter: Mutex::new(filter),
        errors: Mutex::new(errors_map),
        parcel: Mutex::new(parcel_map),
        parcel_max_map: Mutex::new(parcel_max_map),
    });

    if let Some(port) = control_port {
        control::serve(port, state.clone())
            .await
            .with_context(|| format!("bind control server on port {port}"))?;
        println!(
            "bindfetto: control channel on 0.0.0.0:{port} \
             (STATUS/START/STOP/SINK/DLT/ERRORS/PARCEL/TRACK/LIST/GET/SET/CLEAR)"
        );
    }

    let ring = RingBuf::try_from(ebpf.take_map("EVENTS").context("EVENTS map missing")?)?;
    let mut async_ring = AsyncFd::new(ring)?;
    let mut names = NameCache::default();
    // Kernel events carry CLOCK_MONOTONIC ns; this offset maps them to wall-clock.
    let boot_offset_ns = monotonic_to_realtime_offset_ns();
    // For decoding an error's concrete cause (errno) from the kernel's binder log.
    let failed_log = FailedTxLog::resolve();
    if errors_enabled && failed_log.is_none() {
        println!(
            "bindfetto: note — binder failed_transaction_log not found; \
             errors will show the BR_* code without a concrete errno"
        );
    }
    let mut emitter = Emitter::new(state, boot_offset_ns, jsonl, dlt, failed_log);

    if name_debug() {
        eprintln!("bindfetto: BINDFETTO_DEBUG on — tracing pid→name resolution to stderr");
    }
    println!("bindfetto: capturing binder transactions (Ctrl-C to stop)");

    loop {
        let mut guard = async_ring.readable_mut().await?;
        let ring = guard.get_inner_mut();
        while let Some(item) = ring.next() {
            // Two record shapes share the ring (M6): a bare TxEvent, or a variable-length
            // TxRecord (header + `parcel_len` + that many payload bytes). Tell them apart
            // by the item's length so the no-parcel path is byte-identical to before.
            let ev: &TxEvent = unsafe { &*(item.as_ptr() as *const TxEvent) };
            if item.len() > std::mem::size_of::<TxEvent>() {
                // parcel_len sits right after the TxEvent header; payload follows it.
                let poff = std::mem::offset_of!(TxRecord, parcel);
                let parcel_len =
                    unsafe { *(item.as_ptr().add(std::mem::size_of::<TxEvent>()) as *const u32) }
                        as usize;
                // Defensive: never read past what the ring item actually carries.
                let n = parcel_len.min(item.len().saturating_sub(poff));
                let parcel = unsafe { std::slice::from_raw_parts(item.as_ptr().add(poff), n) };
                emitter.emit(ev, Some(parcel), &mut names);
            } else {
                emitter.emit(ev, None, &mut names);
            }
        }
        // Flush the JSONL file once per wakeup so a Ctrl-C loses at most the current
        // (already-drained) batch.
        emitter.flush();
        guard.clear_ready();
    }
}

/// Owns the output config plus buffers reused across every event, so no sink
/// allocates on the heap per line (buffer capacity is retained between calls).
/// The DLT server plus its per-message encode state (reused across events).
struct DltState {
    server: dlt::DltServer,
    counter: u8,
    buf: Vec<u8>,
    ecu: [u8; 4],
    apid: [u8; 4],
    ctid: [u8; 4],
}

impl DltState {
    fn new(server: dlt::DltServer) -> Self {
        Self {
            server,
            counter: 0,
            buf: Vec::new(),
            ecu: dlt_wire::id4("BFTO"),
            apid: dlt_wire::id4("BFTO"),
            ctid: dlt_wire::id4("BIND"),
        }
    }
}

struct Emitter {
    /// Live-tunable runtime state (sink, capture/discovery toggles, DLT gate, counts).
    state: Arc<RuntimeState>,
    boot_offset_ns: i128,
    jsonl: Option<BufWriter<fs::File>>,
    dlt: Option<DltState>,
    /// The kernel's binder failed-transaction log, for decoding an error's concrete errno
    /// (`None` if the log isn't available on this device).
    failed_log: Option<FailedTxLog>,
    /// Interfaces already published to `state.observed`, to skip the shared lock on repeats.
    seen: HashSet<String>,
    core: String,
    scratch: String,
    json: String,
}

impl Emitter {
    fn new(
        state: Arc<RuntimeState>,
        boot_offset_ns: i128,
        jsonl: Option<BufWriter<fs::File>>,
        dlt: Option<DltState>,
        failed_log: Option<FailedTxLog>,
    ) -> Self {
        Self {
            state,
            boot_offset_ns,
            jsonl,
            dlt,
            failed_log,
            seen: HashSet::new(),
            core: String::new(),
            scratch: String::new(),
            json: String::new(),
        }
    }

    /// Emit one transaction to every configured sink. `parcel` carries the raw captured
    /// parcel bytes (M6) when payload capture ran for this transaction, else `None`; it
    /// is rendered as a trailing `parcel=<hex>` token the offline decoder reads.
    fn emit(&mut self, ev: &TxEvent, parcel: Option<&[u8]>, names: &mut NameCache) {
        self.state.captured.fetch_add(1, Ordering::Relaxed);
        // Master capture gate (`STOP`): drop drained events without emitting.
        if !self.state.capturing.load(Ordering::Relaxed) {
            return;
        }
        self.state.emitted.fetch_add(1, Ordering::Relaxed);

        let is_error = ev.is_error();

        // For an error, recover the concrete failure errno from the kernel's
        // failed_transaction_log, matched by transaction debug_id (0/absent → none).
        let errno = if is_error {
            self.failed_log
                .as_mut()
                .and_then(|f| f.errno_for(ev.debug_id))
                .filter(|&e| e != 0)
        } else {
            None
        };

        // Record the interface for control-channel discovery, but only while discovery is
        // on (default off) and for real transactions (an error re-reports an already-seen
        // interface). First sighting of each descriptor takes the shared lock; repeats hit
        // the local `seen` set.
        if !is_error && self.state.discovering.load(Ordering::Relaxed) {
            self.scratch.clear();
            if write_iface(&mut self.scratch, ev) && self.seen.insert(self.scratch.clone()) {
                self.state.observed.lock().unwrap().insert(self.scratch.clone());
            }
        }

        self.core.clear();
        if is_error {
            format_error(&mut self.core, ev, names);
            if let Some(errno) = errno {
                use std::fmt::Write as _;
                match errno_reason(errno) {
                    Some(reason) => {
                        let _ = write!(self.core, " ({reason}, {errno})");
                    }
                    None => {
                        let _ = write!(self.core, " (errno {errno})");
                    }
                }
            }
        } else {
            format_core(&mut self.core, ev, names);
            // Append the raw parcel as a `parcel=<hex>` token so the offline decoder can
            // unmarshal method arguments. Kept out of the error branch (errors carry no
            // payload) and off the on-device decode path by design — bytes only.
            if let Some(bytes) = parcel {
                use std::fmt::Write as _;
                let _ = write!(self.core, " parcel={}/{}:", bytes.len(), ev.data_size);
                push_hex(&mut self.core, bytes);
            }
        }
        let sink = Sink::from_u8(self.state.sink.load(Ordering::Relaxed));
        if sink.console() {
            self.scratch.clear();
            write_timestamp(&mut self.scratch, ev.ts_ns, self.boot_offset_ns);
            self.scratch.push(' ');
            self.scratch.push_str(&self.core);
            println!("{}", self.scratch);
        }
        if sink.logcat() {
            // Logcat records its own timestamp, so the message carries only the marker
            // and the core line. (liblog's C API copies the string, so one alloc here
            // is unavoidable.)
            self.scratch.clear();
            self.scratch.push_str(LOG_MARKER);
            self.scratch.push(' ');
            self.scratch.push_str(&self.core);
            logcat::write(LOG_TAG, &self.scratch);
        }
        if self.dlt.is_some() && self.state.dlt_on.load(Ordering::Relaxed) {
            // Carry the marker + core line (same content as logcat) as a verbose DLT
            // message. DLT stamps reception time; we pass the kernel monotonic time as
            // the message timestamp (0.1 ms units).
            self.scratch.clear();
            self.scratch.push_str(LOG_MARKER);
            self.scratch.push(' ');
            self.scratch.push_str(&self.core);
            let ts_tenths_ms = (ev.ts_ns / 100_000) as u32;
            if let Some(d) = self.dlt.as_mut() {
                dlt_wire::encode(
                    &mut d.buf,
                    d.counter,
                    ts_tenths_ms,
                    &d.ecu,
                    &d.apid,
                    &d.ctid,
                    &self.scratch,
                );
                d.counter = d.counter.wrapping_add(1);
                d.server.send(Arc::from(&d.buf[..]));
            }
        }
        if self.jsonl.is_some() {
            self.write_jsonl(ev, parcel, names, errno);
        }
    }

    /// Append one JSONL record for `ev` to the file sink. The structured fields let
    /// offline decoders read them directly instead of re-parsing the pretty line.
    fn write_jsonl(
        &mut self,
        ev: &TxEvent,
        parcel: Option<&[u8]>,
        names: &mut NameCache,
        errno: Option<i32>,
    ) {
        use std::fmt::Write as _;

        // Decode the interface into `scratch`; absent for replies / non-AIDL.
        self.scratch.clear();
        let has_iface = write_iface(&mut self.scratch, ev);

        names.ensure_with_comm(ev.src_pid, &ev.src_comm);
        names.ensure(ev.dst_pid);
        let src = names.lookup(ev.src_pid);
        let dst = names.lookup(ev.dst_pid);
        let ts_ms = ((ev.ts_ns as i128 + self.boot_offset_ns) / 1_000_000) as i64;

        self.json.clear();
        let j = &mut self.json;
        j.push('{');
        let _ = write!(j, "\"ts_ms\":{ts_ms},\"src\":\"");
        json_escape(j, src);
        let _ = write!(j, "\",\"src_pid\":{},\"dst\":\"", ev.src_pid);
        json_escape(j, dst);
        let _ = write!(
            j,
            "\",\"dst_pid\":{},\"code\":{},\"size\":{},\"oneway\":{},\"reply\":{}",
            ev.dst_pid,
            ev.code,
            ev.data_size,
            ev.is_oneway(),
            ev.reply != 0,
        );
        if has_iface {
            j.push_str(",\"iface\":\"");
            json_escape(j, &self.scratch);
            j.push('"');
        }
        // Raw captured parcel bytes (M6) as hex, for offline argument decoding.
        // `parcel_len` may be < `size` when the payload was truncated at the cap.
        if let Some(bytes) = parcel {
            let _ = write!(j, ",\"parcel_len\":{},\"parcel\":\"", bytes.len());
            push_hex(j, bytes);
            j.push('"');
        }
        // Error events carry the human-readable binder return code; decoders can select
        // them on the `error` key (absent for normal transactions). When the concrete
        // failure errno was recovered, add it plus its decoded reason.
        if ev.is_error() {
            let _ = write!(j, ",\"error\":\"{}\"", br_error_name(ev.err_code));
            if let Some(errno) = errno {
                let _ = write!(j, ",\"errno\":{errno}");
                if let Some(reason) = errno_reason(errno) {
                    j.push_str(",\"reason\":\"");
                    json_escape(j, reason);
                    j.push('"');
                }
            }
        }
        j.push('}');

        if let Some(w) = self.jsonl.as_mut() {
            let _ = writeln!(w, "{}", self.json);
        }
    }

    /// Flush the JSONL file sink, if any.
    fn flush(&mut self) {
        if let Some(w) = self.jsonl.as_mut() {
            let _ = w.flush();
        }
    }
}

/// Write `src (pid) -> dst (pid): <label>` into `out`, where `<label>` is the interface
/// + raw code, a reply marker, or a non-AIDL marker. Shared by the transaction line
/// ([`format_core`]) and the error line ([`format_error`]).
fn write_label(out: &mut String, ev: &TxEvent, names: &mut NameCache) {
    use std::fmt::Write as _;
    names.ensure_with_comm(ev.src_pid, &ev.src_comm);
    names.ensure(ev.dst_pid);
    let src = names.lookup(ev.src_pid);
    let dst = names.lookup(ev.dst_pid);
    let _ = write!(out, "{src} ({}) -> {dst} ({}): ", ev.src_pid, ev.dst_pid);
    // When there's no AIDL interface token: a reply carries none by design; anything
    // else is likely HIDL/hwbinder or a special transaction, not an AIDL call.
    if write_iface(out, ev) {
        let _ = write!(out, ".[code:{}]", ev.code);
    } else if ev.reply != 0 {
        let _ = write!(out, "<reply code:{}>", ev.code);
    } else {
        let _ = write!(out, "<non-aidl code:{}>", ev.code);
    }
}

/// Write the shared transaction line into `out`:
/// `src (pid) -> dst (pid): <label>, <size>B [oneway]`.
fn format_core(out: &mut String, ev: &TxEvent, names: &mut NameCache) {
    use std::fmt::Write as _;
    write_label(out, ev, names);
    let oneway = if ev.is_oneway() { " oneway" } else { "" };
    let _ = write!(out, ", {}B{oneway}", ev.data_size);
}

/// Write an error line into `out`: the failing transaction's label followed by the
/// human-readable binder return error code (`src (pid) -> dst (pid): <label> !! ERR`).
fn format_error(out: &mut String, ev: &TxEvent, names: &mut NameCache) {
    use std::fmt::Write as _;
    write_label(out, ev, names);
    let _ = write!(out, " !! {}", br_error_name(ev.err_code));
}

/// Human-readable name for a captured binder return error code.
/// Reader for the kernel's binder `failed_transaction_log`. The coarse `BR_FAILED_REPLY`
/// code doesn't say *why* a transaction failed; the driver records the concrete errno as
/// `return_error_param` in this ring of the last ~32 failures. We match an error event to
/// its entry by the transaction `debug_id` and recover that errno (e.g. `-ENOSPC` = the
/// target's binder buffer is full). Reading is on-demand and only for the rare error
/// event, so it stays off the hot path.
struct FailedTxLog {
    path: std::path::PathBuf,
    /// `debug_id` → concrete errno, accumulated across reads. The kernel log is a small
    /// ring (~32 entries); during a burst an entry can rotate out before we process its
    /// error event. Slurping the whole ring into this cache on each read means a failure
    /// is decodable as long as we saw it *at some point* while it was in the ring.
    cache: HashMap<i32, i32>,
}

impl FailedTxLog {
    /// Cap the cache so a long session with many errors can't grow it without bound; the
    /// ids are monotonic so a full clear only risks re-missing a handful of in-flight ones.
    const CACHE_CAP: usize = 16384;

    /// Resolve the log path once (binderfs first, then legacy debugfs). `None` if neither
    /// is present — concrete reasons simply won't be attached.
    fn resolve() -> Option<Self> {
        const PATHS: [&str; 2] = [
            "/dev/binderfs/binder_logs/failed_transaction_log",
            "/sys/kernel/debug/binder/failed_transaction_log",
        ];
        PATHS
            .iter()
            .map(std::path::PathBuf::from)
            .find(|p| p.exists())
            .map(|path| FailedTxLog {
                path,
                cache: HashMap::new(),
            })
    }

    /// The concrete failure errno (negative) the kernel recorded for `debug_id`. Checks the
    /// cache first, then re-reads the ring (merging every current entry into the cache)
    /// before looking up again. `None` if the entry has already rotated out unseen.
    fn errno_for(&mut self, debug_id: i32) -> Option<i32> {
        if let Some(&errno) = self.cache.get(&debug_id) {
            return Some(errno);
        }
        if let Ok(content) = fs::read_to_string(&self.path) {
            if self.cache.len() > Self::CACHE_CAP {
                self.cache.clear();
            }
            for line in content.lines() {
                if let Some((id, errno)) = bindfetto_core::parse_failed_tx_entry(line) {
                    self.cache.insert(id, errno);
                }
            }
        }
        self.cache.get(&debug_id).copied()
    }
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

/// Write a kernel monotonic timestamp into `out` as local wall-clock `HH:MM:SS.mmm`.
fn write_timestamp(out: &mut String, ts_ns: u64, boot_offset_ns: i128) {
    use std::fmt::Write as _;
    let wall_ns = ts_ns as i128 + boot_offset_ns;
    let secs = (wall_ns / 1_000_000_000) as i64;
    let nsec = (wall_ns % 1_000_000_000) as u32;
    match chrono::DateTime::from_timestamp(secs, nsec) {
        // `format(..)` yields a Display adapter; writing it borrows-and-formats in
        // place with no intermediate String.
        Some(dt) => {
            let _ = write!(out, "{}", dt.with_timezone(&chrono::Local).format("%H:%M:%S%.3f"));
        }
        None => out.push_str("--:--:--.---"),
    }
}

/// Decode the event's UTF-16LE interface descriptor and append it to `out`. Returns false
/// (writing nothing) when the event carries no usable descriptor. Thin wrapper over
/// [`bindfetto_core::write_iface_bytes`] with the event's captured buffer.
fn write_iface(out: &mut String, ev: &TxEvent) -> bool {
    write_iface_bytes(out, &ev.iface, ev.iface_byte_len as usize)
}

/// pid -> process name, cached (a pid's name is stable for its lifetime).
#[derive(Default)]
struct NameCache(HashMap<u32, String>);

impl NameCache {
    /// Resolve and cache `pid`'s name. Re-resolves while the cached value is still the
    /// `pid:<n>` fallback: the first sighting can race a process that was mid-fork/exec
    /// (no `cmdline`/`comm` yet) or had just exited, and caching that failure permanently
    /// would keep mislabeling a pid that later became — or already is — resolvable. A
    /// genuinely resolved name is stable for the pid's lifetime, so it's cached once.
    fn ensure(&mut self, pid: u32) {
        self.ensure_with_comm(pid, &[0u8; 16]);
    }

    /// Like [`ensure`], but when `/proc` resolution fails (a short-lived sender that
    /// already exited), fall back to `comm` — the sender's name the probe captured live
    /// in-kernel — instead of the bare `pid:<n>`. `comm` all-zero means none was captured
    /// (e.g. the destination pid, which has no in-kernel comm).
    fn ensure_with_comm(&mut self, pid: u32, comm: &[u8; 16]) {
        let unresolved = match self.0.get(&pid) {
            None => true,
            Some(name) => is_pid_fallback(name, pid),
        };
        if unresolved {
            let mut name = resolve_name(pid);
            // /proc failed (process gone) but we captured the sender's name in the probe.
            if is_pid_fallback(&name, pid) {
                if let Some(from_comm) = comm_name(comm) {
                    name = from_comm;
                }
            }
            if name_debug() {
                let cached = self.0.get(&pid).map(String::as_str);
                eprintln!("[names] pid {pid}: resolve (was {cached:?}) -> {name:?}");
            }
            self.0.insert(pid, name);
        }
    }

    /// Look up a name already cached by [`ensure`]. Splitting resolution (`&mut`)
    /// from lookup (`&`) lets a caller hold `&str`s for two pids at once without
    /// cloning them out of the map.
    fn lookup(&self, pid: u32) -> &str {
        self.0.get(&pid).map(String::as_str).unwrap_or("?")
    }
}

/// Whether `BINDFETTO_DEBUG` is set (checked once). Enables stderr tracing of pid→name
/// resolution so a stuck `pid:<n>` can be diagnosed without recompiling.
fn name_debug() -> bool {
    use std::sync::OnceLock;
    static D: OnceLock<bool> = OnceLock::new();
    *D.get_or_init(|| std::env::var_os("BINDFETTO_DEBUG").is_some())
}

fn resolve_name(pid: u32) -> String {
    let dbg = name_debug();
    // /proc/<pid>/cmdline: NUL-separated argv; the first field is the process name.
    match fs::read(format!("/proc/{pid}/cmdline")) {
        Ok(bytes) => {
            let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
            if end > 0 {
                return String::from_utf8_lossy(&bytes[..end]).into_owned();
            }
            if dbg {
                eprintln!("[names] pid {pid}: cmdline read ok but empty ({} bytes)", bytes.len());
            }
        }
        Err(e) if dbg => eprintln!("[names] pid {pid}: cmdline read failed: {e}"),
        Err(_) => {}
    }
    // Fallback: /proc/<pid>/comm (truncated to 15 chars by the kernel).
    match fs::read_to_string(format!("/proc/{pid}/comm")) {
        Ok(s) => {
            let t = s.trim_end();
            if !t.is_empty() {
                return t.to_owned();
            }
            if dbg {
                eprintln!("[names] pid {pid}: comm read ok but empty");
            }
        }
        Err(e) if dbg => eprintln!("[names] pid {pid}: comm read failed: {e}"),
        Err(_) => {}
    }
    if dbg {
        // Is the pid actually alive right now? Distinguishes an exit race from a genuine
        // read failure on a live process (permissions / namespace / procfs quirk).
        let alive = std::path::Path::new(&format!("/proc/{pid}")).exists();
        eprintln!("[names] pid {pid}: UNRESOLVED (/proc/{pid} exists: {alive})");
    }
    format!("pid:{pid}")
}

/// True if `name` is the unresolved `pid:<pid>` fallback from [`resolve_name`] (so the
/// cache should keep retrying) rather than a real process name.
fn is_pid_fallback(name: &str, pid: u32) -> bool {
    name.strip_prefix("pid:")
        .and_then(|n| n.parse::<u32>().ok())
        == Some(pid)
}
