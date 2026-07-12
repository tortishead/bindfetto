//! bindfetto-decode: offline method-name decoding for bindfetto logs.
//!
//! The on-device runtime emits transaction lines with the *raw* transaction code,
//! because resolving method names on the hot path would be expensive and would tie
//! the logs to one catalog version:
//!
//! ```text
//! com.example.app (1234) -> system_server (5678): android.app.IActivityManager.[code:7], 512B
//! ```
//!
//! This crate is the offline decode step. Given a precompiled AIDL catalog
//! (`interface` → `code` → `method`), it rewrites each `interface.[code:N]` token
//! into `interface.method`:
//!
//! ```text
//! com.example.app (1234) -> system_server (5678): android.app.IActivityManager.startActivity, 512B
//! ```
//!
//! It is the plugin-agnostic core the SPEC calls for: the `bindfetto-decode` CLI,
//! the DLT Viewer plugin (via a C ABI), and the VS Code extension (via WASM) are all
//! thin adapters over [`Decoder`]. The line rewrite ([`Decoder::decode_line`]) is
//! prefix-agnostic — it works whether the line arrives bare, with a console
//! timestamp, behind the `BINDFETTO` marker, or wrapped in logcat/DLT metadata —
//! because it substitutes the token in place and leaves the rest untouched.

mod catalog;
pub mod ffi;
mod parcel;
mod parse;

pub use catalog::{special_transaction, Arg, Catalog};
pub use parse::{Label, Record};

use std::borrow::Cow;

/// The token the runtime emits in the method slot: `.[code:<N>]`.
const CODE_MARK: &str = ".[code:";

/// The token the runtime appends when parcel capture is on (M6):
/// ` parcel=<captured>/<total>:<hex>`. The decoder renders the bytes into method
/// arguments and drops the raw token from the line.
const PARCEL_MARK: &str = " parcel=";

/// A catalog plus the logic to resolve and rewrite transaction codes.
pub struct Decoder {
    catalog: Catalog,
}

impl Decoder {
    /// Build a decoder from an already-loaded catalog.
    pub fn new(catalog: Catalog) -> Self {
        Self { catalog }
    }

    /// Build a decoder from catalog JSON (see [`Catalog::from_json`]).
    pub fn from_catalog_json(json: &str) -> Result<Self, serde_json::Error> {
        Ok(Self::new(Catalog::from_json(json)?))
    }

    /// The underlying catalog.
    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    /// Resolve `(interface, code)` to a method name. Well-known special
    /// transactions (PING/DUMP/…) resolve for any interface; everything else is a
    /// catalog lookup. Returns `None` for an unknown code.
    pub fn method(&self, iface: &str, code: u32) -> Option<&str> {
        if let Some(name) = special_transaction(code) {
            return Some(name);
        }
        self.catalog.method(iface, code)
    }

    /// Rewrite every `interface.[code:N]` token in `line` whose method is known,
    /// leaving the rest of the line — and any unknown codes — exactly as-is. When the
    /// line carries a captured parcel (`parcel=<hex>`, M6) and the resolved method has
    /// known argument types, the arguments are rendered after the method name
    /// (`method(a=1, b="x")`) and the raw parcel token is dropped from the output.
    ///
    /// Returns [`Cow::Borrowed`] when nothing changed, so a stream of non-bindfetto
    /// lines passes through without allocating.
    pub fn decode_line<'a>(&self, line: &'a str) -> Cow<'a, str> {
        // The captured parcel bytes (decoded once) and the span to strip if we render
        // them into arguments; `None` when the line carries no parcel token.
        let parcel = parse_parcel(line);
        let mut rendered_parcel = false;

        let mut out: Option<String> = None;
        // Bytes of `line` already flushed into `out` (only meaningful once `out` is
        // set); everything from here to the next replacement is copied verbatim.
        let mut copied = 0;
        let mut search = 0;

        while let Some(rel) = line[search..].find(CODE_MARK) {
            let dot = search + rel; // the '.' of ".[code:"
            let code_start = dot + CODE_MARK.len();
            // Advance search past this marker regardless of whether it resolves.
            search = code_start;

            let Some((code, code_end)) = parse_code(line, code_start) else {
                continue;
            };
            let iface = iface_before(line, dot);
            if iface.is_empty() {
                continue;
            }
            let Some(method) = self.method(iface, code) else {
                continue;
            };

            let iface_start = dot - iface.len();
            let buf = out.get_or_insert_with(|| String::with_capacity(line.len()));
            buf.push_str(&line[copied..iface_start]);
            buf.push_str(iface);
            buf.push('.');
            buf.push_str(method);
            // Render arguments from the captured parcel, once, for the first method that
            // has a known non-empty signature.
            if !rendered_parcel {
                if let Some(bytes) = parcel.as_ref().map(|p| &p.bytes) {
                    if let Some(args) = self.catalog.args(iface, code) {
                        if !args.is_empty() && parcel::render_args(buf, args, bytes) {
                            rendered_parcel = true;
                        }
                    }
                }
            }
            copied = code_end; // skip the original ".[code:N]"
            search = code_end;
        }

        match out {
            Some(mut buf) => {
                match (&parcel, rendered_parcel) {
                    // Rendered the args — drop the now-redundant raw parcel token.
                    (Some(p), true) => {
                        buf.push_str(&line[copied..p.start]);
                        buf.push_str(&line[p.end..]);
                    }
                    _ => buf.push_str(&line[copied..]),
                }
                Cow::Owned(buf)
            }
            None => Cow::Borrowed(line),
        }
    }
}

/// A captured parcel token located in a line: the decoded bytes and the `[start, end)`
/// span of the raw ` parcel=…` token (including its leading space) to strip on render.
struct Parcel {
    start: usize,
    end: usize,
    bytes: Vec<u8>,
}

/// Locate and decode a ` parcel=<captured>/<total>:<hex>` token, if present.
fn parse_parcel(line: &str) -> Option<Parcel> {
    let start = line.find(PARCEL_MARK)?;
    let after = &line[start + PARCEL_MARK.len()..];
    // Skip the "<captured>/<total>:" prefix; the hex payload follows the ':'.
    let colon = after.find(':')?;
    let hex = &after[colon + 1..];
    let hex_len = hex
        .find(|c: char| !c.is_ascii_hexdigit())
        .unwrap_or(hex.len());
    let hex = &hex[..hex_len];
    let end = start + PARCEL_MARK.len() + colon + 1 + hex_len;
    Some(Parcel {
        start,
        end,
        bytes: decode_hex(hex),
    })
}

/// Decode an even-length ASCII hex string to bytes; a trailing half-byte is ignored.
fn decode_hex(hex: &str) -> Vec<u8> {
    let b = hex.as_bytes();
    let mut out = Vec::with_capacity(b.len() / 2);
    let mut i = 0;
    while i + 1 < b.len() {
        let hi = (b[i] as char).to_digit(16);
        let lo = (b[i + 1] as char).to_digit(16);
        match (hi, lo) {
            (Some(h), Some(l)) => out.push((h * 16 + l) as u8),
            _ => break,
        }
        i += 2;
    }
    out
}

/// Parse the `N]` that follows `.[code:`, starting at `at`. Returns the code and the
/// index just past the closing `]`.
fn parse_code(line: &str, at: usize) -> Option<(u32, usize)> {
    let bytes = line.as_bytes();
    let mut i = at;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == at || i >= bytes.len() || bytes[i] != b']' {
        return None; // no digits, or not closed by ']'
    }
    let code = line[at..i].parse().ok()?;
    Some((code, i + 1))
}

/// The interface descriptor immediately before `end` (the '.' of `.[code:`): the run
/// of `[A-Za-z0-9_.]` ending there.
fn iface_before(line: &str, end: usize) -> &str {
    let bytes = line.as_bytes();
    let mut start = end;
    while start > 0 {
        let c = bytes[start - 1];
        if c == b'_' || c == b'.' || c.is_ascii_alphanumeric() {
            start -= 1;
        } else {
            break;
        }
    }
    // A leading '.' isn't part of the descriptor (e.g. the separator run collapsed).
    let s = &line[start..end];
    s.trim_start_matches('.')
}
