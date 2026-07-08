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
//! * `--dlt` injects the marked lines straight into the DLT daemon (via libdlt), so
//!   DLT Viewer shows them **live** even without an OEM logcat->DLT bridge.
//!
//! Both compose with any `--sink` (use `--sink none` for a quiet, sink-only capture).

use std::collections::HashMap;
use std::fs;
use std::io::{BufWriter, Write as _};

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

/// Optional DLT (Diagnostic Log and Trace) sink for automotive targets.
///
/// libdlt isn't in the NDK and is present only where the OEM ships DLT, so it's
/// resolved at runtime via `dlopen` — the binary always builds, and `--dlt` only
/// activates where `libdlt` + a running `dlt-daemon` exist. Injecting directly lets
/// DLT Viewer show bindfetto's traffic **live** even when there's no logcat->DLT
/// bridge to carry it.
mod dlt {
    use std::os::raw::{c_char, c_int, c_void};

    use std::ffi::CString;

    /// `DLT_LOG_INFO` from dlt's `DltLogLevelType`.
    const DLT_LOG_INFO: c_int = 4;

    // libdlt C API (dlt_user.h). DltContext is passed by pointer; we hand libdlt an
    // over-allocated, zeroed buffer so its exact layout/size need not be known here.
    type RegisterApp = unsafe extern "C" fn(*const c_char, *const c_char) -> c_int;
    type RegisterContext = unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> c_int;
    type LogString = unsafe extern "C" fn(*mut c_void, c_int, *const c_char) -> c_int;
    type UnregisterContext = unsafe extern "C" fn(*mut c_void) -> c_int;
    type UnregisterApp = unsafe extern "C" fn() -> c_int;

    /// A live DLT registration. Logs each line under app/context ids to the daemon.
    pub struct Dlt {
        lib: *mut c_void,
        // libdlt keeps using this after registration, so it must outlive logging and
        // never move — hence a heap box. 256 zeroed bytes comfortably covers DltContext.
        ctx: Box<[u8; 256]>,
        log_string: LogString,
        unregister_context: UnregisterContext,
        unregister_app: UnregisterApp,
    }

    impl Dlt {
        /// Load libdlt, register `appid`/`ctxid` (each <= 4 chars), ready to log.
        pub fn open(appid: &str, ctxid: &str) -> Result<Self, String> {
            unsafe {
                let lib = dlopen_any(&["libdlt.so", "libdlt.so.2"])?;
                let register_app: RegisterApp = sym(lib, "dlt_register_app")?;
                let register_context: RegisterContext = sym(lib, "dlt_register_context")?;
                let log_string: LogString = sym(lib, "dlt_log_string")?;
                let unregister_context: UnregisterContext = sym(lib, "dlt_unregister_context")?;
                let unregister_app: UnregisterApp = sym(lib, "dlt_unregister_app")?;

                let app = CString::new(appid).map_err(|_| "invalid DLT app id")?;
                let cid = CString::new(ctxid).map_err(|_| "invalid DLT context id")?;
                let desc = CString::new("bindfetto").unwrap();

                let mut ctx = Box::new([0u8; 256]);
                register_app(app.as_ptr(), desc.as_ptr());
                register_context(ctx.as_mut_ptr() as *mut c_void, cid.as_ptr(), desc.as_ptr());

                Ok(Dlt {
                    lib,
                    ctx,
                    log_string,
                    unregister_context,
                    unregister_app,
                })
            }
        }

        /// Send one line to the DLT daemon at INFO level.
        pub fn log(&mut self, msg: &str) {
            if let Ok(c) = CString::new(msg) {
                unsafe {
                    (self.log_string)(self.ctx.as_mut_ptr() as *mut c_void, DLT_LOG_INFO, c.as_ptr());
                }
            }
        }
    }

    impl Drop for Dlt {
        fn drop(&mut self) {
            unsafe {
                (self.unregister_context)(self.ctx.as_mut_ptr() as *mut c_void);
                (self.unregister_app)();
                libc::dlclose(self.lib);
            }
        }
    }

    unsafe fn dlopen_any(names: &[&str]) -> Result<*mut c_void, String> {
        for name in names {
            let c = CString::new(*name).unwrap();
            let handle = libc::dlopen(c.as_ptr(), libc::RTLD_NOW);
            if !handle.is_null() {
                return Ok(handle);
            }
        }
        Err(format!(
            "libdlt not found (tried {names:?}); the DLT sink needs libdlt + a dlt-daemon on the target"
        ))
    }

    /// Resolve a libdlt symbol into a typed function pointer.
    unsafe fn sym<T>(lib: *mut c_void, name: &str) -> Result<T, String> {
        let c = CString::new(name).unwrap();
        let ptr = libc::dlsym(lib, c.as_ptr());
        if ptr.is_null() {
            return Err(format!("libdlt is missing symbol {name}"));
        }
        // dlsym yields a function address; T is a pointer-sized `extern "C" fn`.
        Ok(std::mem::transmute_copy::<*mut c_void, T>(&ptr))
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
    let dlt = if args.iter().any(|a| a == "--dlt") {
        Some(dlt::Dlt::open("BFTO", "BIND").map_err(anyhow::Error::msg).context("enable DLT sink")?)
    } else {
        None
    };

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
struct Emitter {
    sink: Sink,
    boot_offset_ns: i128,
    jsonl: Option<BufWriter<fs::File>>,
    dlt: Option<dlt::Dlt>,
    core: String,
    scratch: String,
    json: String,
}

impl Emitter {
    fn new(
        sink: Sink,
        boot_offset_ns: i128,
        jsonl: Option<BufWriter<fs::File>>,
        dlt: Option<dlt::Dlt>,
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
            // DLT records its own timestamp; carry the marker + core line, same as logcat.
            self.scratch.clear();
            self.scratch.push_str(LOG_MARKER);
            self.scratch.push(' ');
            self.scratch.push_str(&self.core);
            if let Some(d) = self.dlt.as_mut() {
                d.log(&self.scratch);
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
