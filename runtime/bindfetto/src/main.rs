//! Bindfetto userspace consumer (M1–M3).
//!
//! Loads the probe, attaches the kprobe + tracepoint, drains the ring buffer, and
//! emits one line per transaction to the selected sink:
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

mod dlt_wire;

use std::collections::HashMap;
use std::fs;
use std::io::{BufWriter, Write as _};
use std::sync::Arc;

use anyhow::Context as _;
use aya::{
    maps::{Array, HashMap as BpfHashMap, RingBuf},
    programs::{KProbe, TracePoint},
    Ebpf,
};
use bindfetto_common::{IfaceKey, TxEvent, MAX_IFACE_BYTES};
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
        match arg_value(args, "--sink") {
            Some("logcat") => Sink::Logcat,
            Some("both") => Sink::Both,
            Some("none") => Sink::None,
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

/// Build the in-kernel filter key for an interface name: its UTF-16LE bytes,
/// zero-padded to [`MAX_IFACE_BYTES`] — byte-identical to what the probe captures
/// into `TxEvent::iface`, so a direct map lookup matches. Names longer than the
/// buffer are truncated (the probe truncates the same way).
fn iface_key(name: &str) -> IfaceKey {
    let mut key = [0u8; MAX_IFACE_BYTES];
    let mut i = 0;
    for unit in name.encode_utf16() {
        if i + 2 > MAX_IFACE_BYTES {
            break;
        }
        let [lo, hi] = unit.to_le_bytes();
        key[i] = lo;
        key[i + 1] = hi;
        i += 2;
    }
    IfaceKey(key)
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

    let args: Vec<String> = std::env::args().collect();
    let sink = Sink::parse(&args);
    let jsonl = match arg_value(&args, "--jsonl") {
        Some(path) => Some(BufWriter::new(
            fs::File::create(path).with_context(|| format!("create jsonl file {path}"))?,
        )),
        None => None,
    };
    let dlt = match args.iter().position(|a| a == "--dlt-serve") {
        Some(i) => {
            let port = args
                .get(i + 1)
                .and_then(|s| s.parse::<u16>().ok())
                .unwrap_or(3490);
            let server = dlt::DltServer::bind(port)
                .await
                .with_context(|| format!("bind DLT server on port {port}"))?;
            println!("bindfetto: DLT server on 0.0.0.0:{port} — connect DLT Viewer as a TCP ECU");
            Some(DltState::new(server))
        }
        None => None,
    };

    // In-kernel interface filter (M4): push the wanted descriptors into the BPF map and
    // flip the enable flag, so non-matching transactions are dropped in the probe before
    // they ever reach the ring buffer.
    let ifaces = iface_filters(&args);
    if !ifaces.is_empty() {
        let mut wanted: BpfHashMap<_, IfaceKey, u8> =
            BpfHashMap::try_from(ebpf.map_mut("WANTED").context("WANTED map missing")?)?;
        for name in &ifaces {
            wanted
                .insert(iface_key(name), 1u8, 0)
                .with_context(|| format!("insert interface filter {name}"))?;
        }
        let mut filter_on: Array<_, u32> =
            Array::try_from(ebpf.map_mut("FILTER_ON").context("FILTER_ON map missing")?)?;
        filter_on.set(0, 1u32, 0).context("enable interface filter")?;
        println!(
            "bindfetto: in-kernel interface filter active — keeping {}: {}",
            ifaces.len(),
            ifaces.join(", ")
        );
    }

    let ring = RingBuf::try_from(ebpf.take_map("EVENTS").context("EVENTS map missing")?)?;
    let mut async_ring = AsyncFd::new(ring)?;
    let mut names = NameCache::default();
    // Kernel events carry CLOCK_MONOTONIC ns; this offset maps them to wall-clock.
    let boot_offset_ns = monotonic_to_realtime_offset_ns();
    let mut emitter = Emitter::new(sink, boot_offset_ns, jsonl, dlt);

    println!("bindfetto: capturing binder transactions (Ctrl-C to stop)");

    loop {
        let mut guard = async_ring.readable_mut().await?;
        let ring = guard.get_inner_mut();
        while let Some(item) = ring.next() {
            let ev: &TxEvent = unsafe { &*(item.as_ptr() as *const TxEvent) };
            emitter.emit(ev, &mut names);
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
    sink: Sink,
    boot_offset_ns: i128,
    jsonl: Option<BufWriter<fs::File>>,
    dlt: Option<DltState>,
    core: String,
    scratch: String,
    json: String,
}

impl Emitter {
    fn new(
        sink: Sink,
        boot_offset_ns: i128,
        jsonl: Option<BufWriter<fs::File>>,
        dlt: Option<DltState>,
    ) -> Self {
        Self {
            sink,
            boot_offset_ns,
            jsonl,
            dlt,
            core: String::new(),
            scratch: String::new(),
            json: String::new(),
        }
    }

    /// Emit one transaction to every configured sink.
    fn emit(&mut self, ev: &TxEvent, names: &mut NameCache) {
        self.core.clear();
        format_core(&mut self.core, ev, names);
        if self.sink.console() {
            self.scratch.clear();
            write_timestamp(&mut self.scratch, ev.ts_ns, self.boot_offset_ns);
            self.scratch.push(' ');
            self.scratch.push_str(&self.core);
            println!("{}", self.scratch);
        }
        if self.sink.logcat() {
            // Logcat records its own timestamp, so the message carries only the marker
            // and the core line. (liblog's C API copies the string, so one alloc here
            // is unavoidable.)
            self.scratch.clear();
            self.scratch.push_str(LOG_MARKER);
            self.scratch.push(' ');
            self.scratch.push_str(&self.core);
            logcat::write(LOG_TAG, &self.scratch);
        }
        if self.dlt.is_some() {
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
            self.write_jsonl(ev, names);
        }
    }

    /// Append one JSONL record for `ev` to the file sink. The structured fields let
    /// offline decoders read them directly instead of re-parsing the pretty line.
    fn write_jsonl(&mut self, ev: &TxEvent, names: &mut NameCache) {
        use std::fmt::Write as _;

        // Decode the interface into `scratch`; absent for replies / non-AIDL.
        self.scratch.clear();
        let has_iface = write_iface(&mut self.scratch, ev);

        names.ensure(ev.src_pid);
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

/// Append `s` to `out` as the interior of a JSON string (no surrounding quotes),
/// escaping per RFC 8259.
fn json_escape(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0C}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                use std::fmt::Write as _;
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
}

/// Write the shared, sink-independent line into `out`:
/// `src (pid) -> dst (pid): <label>, <size>B`.
fn format_core(out: &mut String, ev: &TxEvent, names: &mut NameCache) {
    use std::fmt::Write as _;
    names.ensure(ev.src_pid);
    names.ensure(ev.dst_pid);
    let src = names.lookup(ev.src_pid);
    let dst = names.lookup(ev.dst_pid);
    let oneway = if ev.is_oneway() { " oneway" } else { "" };
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
    let _ = write!(out, ", {}B{oneway}", ev.data_size);
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

/// Decode the event's UTF-16LE interface descriptor and append it to `out`.
/// Returns false (writing nothing) when the event carries no usable descriptor.
fn write_iface(out: &mut String, ev: &TxEvent) -> bool {
    let len = ev.iface_byte_len as usize;
    if len == 0 || len > ev.iface.len() {
        return false;
    }
    let units = ev.iface[..len]
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]));
    let start = out.len();
    for ch in char::decode_utf16(units) {
        match ch {
            Ok('\0') => break, // NUL-terminated descriptor: stop at the first NUL
            Ok(c) => out.push(c),
            Err(_) => out.push('\u{FFFD}'),
        }
    }
    out.len() != start
}

/// pid -> process name, cached (a pid's name is stable for its lifetime).
#[derive(Default)]
struct NameCache(HashMap<u32, String>);

impl NameCache {
    /// Resolve and cache `pid`'s name if not already known.
    fn ensure(&mut self, pid: u32) {
        self.0.entry(pid).or_insert_with(|| resolve_name(pid));
    }

    /// Look up a name already cached by [`ensure`]. Splitting resolution (`&mut`)
    /// from lookup (`&`) lets a caller hold `&str`s for two pids at once without
    /// cloning them out of the map.
    fn lookup(&self, pid: u32) -> &str {
        self.0.get(&pid).map(String::as_str).unwrap_or("?")
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
