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
mod parse;

pub use catalog::{special_transaction, Catalog};
pub use parse::{Label, Record};

use std::borrow::Cow;

/// The token the runtime emits in the method slot: `.[code:<N>]`.
const CODE_MARK: &str = ".[code:";

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
    /// leaving the rest of the line — and any unknown codes — exactly as-is.
    ///
    /// Returns [`Cow::Borrowed`] when nothing changed, so a stream of non-bindfetto
    /// lines passes through without allocating.
    pub fn decode_line<'a>(&self, line: &'a str) -> Cow<'a, str> {
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
            copied = code_end; // skip the original ".[code:N]"
            search = code_end;
        }

        match out {
            Some(mut buf) => {
                buf.push_str(&line[copied..]);
                Cow::Owned(buf)
            }
            None => Cow::Borrowed(line),
        }
    }
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
