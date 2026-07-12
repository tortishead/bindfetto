//! Minimal Binder `Parcel` reader for offline argument decoding (M6).
//!
//! The on-device probe captures raw parcel bytes from offset 0 (the interface-token
//! header plus the marshalled arguments) up to a fixed cap, and stays otherwise dumb.
//! All structure lives here: this reader skips the header the same way the probe framed
//! it, then unmarshals arguments following Binder's marshalling rules (little-endian,
//! 4-byte alignment). The payload is capped/partial by design, so every read is
//! truncation-aware — it stops at the captured end and the caller marks the tail.
//!
//! Only fixed-layout types are decoded (integers, float/double, `String16`). Types with
//! a size we can't determine from the catalog alone (binders, arrays, parcelables) end
//! decoding for that call, because we can't skip past them without risking misalignment.

use crate::catalog::Arg;

/// `'SYST'` (little-endian) — the header magic `writeInterfaceToken` writes at parcel
/// offset 8, mirroring the probe's `IFACE_HEADER_MAGIC`.
const HEADER_MAGIC: u32 = 0x5359_5354;

/// Round `n` up to the next multiple of 4 (Parcel's alignment unit).
fn round4(n: usize) -> usize {
    (n + 3) & !3
}

/// A cursor over captured parcel bytes.
pub struct ParcelReader<'a> {
    buf: &'a [u8],
    pos: usize,
    /// Set once a read runs past the captured bytes (payload was truncated at the cap).
    truncated: bool,
}

impl<'a> ParcelReader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self {
            buf,
            pos: 0,
            truncated: false,
        }
    }

    /// True if any read ran off the end of the captured bytes.
    pub fn truncated(&self) -> bool {
        self.truncated
    }

    /// Position the cursor at the first argument, just past the interface-token header:
    /// magic at offset 8, UTF-16 length (code units) at 12, descriptor at 16 (padded to
    /// 4, incl. null terminator). Returns false if the header isn't an AIDL token — then
    /// there are no decodable arguments.
    pub fn seek_args(&mut self) -> bool {
        let Some(magic) = self.peek_u32(8) else {
            return false;
        };
        if magic != HEADER_MAGIC {
            return false;
        }
        let Some(units) = self.peek_u32(12) else {
            return false;
        };
        // (units + 1) char16 including the null terminator, padded to 4 bytes.
        let str_bytes = round4((units as usize + 1) * 2);
        self.pos = 16 + str_bytes;
        true
    }

    /// Read a little-endian u32 at absolute offset `off` without moving the cursor.
    fn peek_u32(&self, off: usize) -> Option<u32> {
        let end = off.checked_add(4)?;
        let b = self.buf.get(off..end)?;
        Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// Consume `n` bytes at the cursor, or mark truncation and return `None`.
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        match self.buf.get(self.pos..end) {
            Some(s) => {
                self.pos = end;
                Some(s)
            }
            None => {
                self.truncated = true;
                None
            }
        }
    }

    fn read_i32(&mut self) -> Option<i32> {
        let b = self.take(4)?;
        Some(i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn read_i64(&mut self) -> Option<i64> {
        let b = self.take(8)?;
        Some(i64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    fn read_f32(&mut self) -> Option<f32> {
        let b = self.take(4)?;
        Some(f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn read_f64(&mut self) -> Option<f64> {
        let b = self.take(8)?;
        Some(f64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    /// Read a `String16`: an int32 code-unit count (`-1` = null), then `(len+1)*2` bytes
    /// (chars + null) padded to 4. Returns `None` on truncation, `Some(None)` for null.
    fn read_string16(&mut self) -> Option<Option<String>> {
        let len = self.read_i32()?;
        if len < 0 {
            return Some(None);
        }
        let len = len as usize;
        let padded = round4((len + 1) * 2);
        let bytes = self.take(padded)?;
        let units: Vec<u16> = bytes[..len * 2]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        Some(Some(String::from_utf16_lossy(&units)))
    }
}

/// Strip AIDL argument decorations down to the base type: direction (`in`/`out`/
/// `inout`), a leading `@nullable`/`@utf8InCpp` annotation, and any trailing generic or
/// array suffix. Returns the lowercase-insensitive base token.
fn base_type(ty: &str) -> &str {
    let mut t = ty.trim();
    loop {
        let stripped = t
            .strip_prefix("in ")
            .or_else(|| t.strip_prefix("out "))
            .or_else(|| t.strip_prefix("inout "))
            .or_else(|| t.strip_prefix("@nullable "))
            .or_else(|| t.strip_prefix("@utf8InCpp "));
        match stripped {
            Some(rest) => t = rest.trim_start(),
            None => break,
        }
    }
    t
}

/// Render the arguments of one call from its captured parcel bytes into `(a=1, b="x")`
/// form, appended to `out`. Decoding stops early — with a trailing note — at the first
/// argument whose type has no fixed wire layout (binder, array, parcelable, …) or when
/// the captured payload runs out; either way the rest can't be safely unmarshalled.
///
/// `parcel` is the raw bytes from offset 0. Returns `false` (leaving `out` unchanged) if
/// the payload carries no interface token, so the caller can fall back to the raw form.
pub fn render_args(out: &mut String, args: &[Arg], parcel: &[u8]) -> bool {
    use std::fmt::Write as _;

    let mut r = ParcelReader::new(parcel);
    if !r.seek_args() {
        return false;
    }

    out.push('(');
    let mut note: Option<&str> = None;
    for (i, arg) in args.iter().enumerate() {
        // Roll-back point: on truncation we drop this half-written `name=` (and its
        // leading separator) so the tail reads cleanly as `…(truncated)`.
        let mark = out.len();
        if i > 0 {
            out.push_str(", ");
        }
        let _ = write!(out, "{}=", arg.name);

        match base_type(&arg.ty) {
            "boolean" => match r.read_i32() {
                Some(v) => out.push_str(if v != 0 { "true" } else { "false" }),
                None => {
                    out.truncate(mark);
                    break;
                }
            },
            // Parcel widens byte/char/short to int32 on the wire.
            "int" | "byte" | "char" | "short" => match r.read_i32() {
                Some(v) => {
                    let _ = write!(out, "{v}");
                }
                None => {
                    out.truncate(mark);
                    break;
                }
            },
            "long" => match r.read_i64() {
                Some(v) => {
                    let _ = write!(out, "{v}");
                }
                None => {
                    out.truncate(mark);
                    break;
                }
            },
            "float" => match r.read_f32() {
                Some(v) => {
                    let _ = write!(out, "{v}");
                }
                None => {
                    out.truncate(mark);
                    break;
                }
            },
            "double" => match r.read_f64() {
                Some(v) => {
                    let _ = write!(out, "{v}");
                }
                None => {
                    out.truncate(mark);
                    break;
                }
            },
            "String" | "CharSequence" => match r.read_string16() {
                Some(Some(s)) => {
                    out.push('"');
                    push_escaped(out, &s);
                    out.push('"');
                }
                Some(None) => out.push_str("null"),
                None => {
                    out.truncate(mark);
                    break;
                }
            },
            // No fixed layout we can skip past — stop before we misalign.
            other => {
                let _ = write!(out, "<{other}>");
                note = Some("unparsed");
                break;
            }
        }
    }

    if r.truncated() {
        if !out.ends_with('(') {
            out.push_str(", ");
        }
        out.push_str("…(truncated)");
    } else if let Some(n) = note {
        let _ = write!(out, ", …({n})");
    }
    out.push(')');
    true
}

/// Append `s` with control chars and quotes escaped, and clamp very long strings so a
/// single argument can't dominate the line.
fn push_escaped(out: &mut String, s: &str) {
    const MAX: usize = 120;
    for (i, c) in s.chars().enumerate() {
        if i >= MAX {
            out.push('…');
            break;
        }
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push('.'),
            c => out.push(c),
        }
    }
}
