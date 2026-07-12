//! Pure, platform-independent logic shared by the bindfetto consumer.
//!
//! Everything here is `std`-only — no `aya`, no Android liblog, no `/proc` — so it can be
//! unit-tested on the host. The consumer (`bindfetto`) links this crate and keeps only
//! the platform-bound glue (BPF maps, process-name resolution, the clock, the sinks).
//!
//! These functions carry cross-component contracts that a silent bug would corrupt
//! rather than crash: [`iface_key`] must match the probe's captured descriptor
//! byte-for-byte or the in-kernel filter matches nothing; [`dlt::encode`] must match what
//! DLT Viewer parses; [`write_iface_bytes`] decodes the UTF-16 the probe captured.

pub mod dlt;

use std::fmt::Write as _;

use bindfetto_common::{
    IfaceKey, BR_DEAD_REPLY, BR_FAILED_REPLY, BR_FROZEN_REPLY, MAX_IFACE_BYTES,
};

/// Build the in-kernel filter key for an interface name: its UTF-16LE bytes, zero-padded
/// to [`MAX_IFACE_BYTES`] — byte-identical to what the probe captures into
/// `TxEvent::iface`, so a direct map lookup matches. Names longer than the buffer are
/// truncated (the probe truncates the same way).
pub fn iface_key(name: &str) -> IfaceKey {
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

/// Decode a captured UTF-16LE interface descriptor (`iface[..byte_len]`) and append it to
/// `out`. Returns false (writing nothing) when there's no usable descriptor. Stops at the
/// first NUL, and substitutes U+FFFD for unpaired surrogates.
pub fn write_iface_bytes(out: &mut String, iface: &[u8], byte_len: usize) -> bool {
    if byte_len == 0 || byte_len > iface.len() {
        return false;
    }
    let units = iface[..byte_len]
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

/// Append `bytes` to `out` as lowercase hex (two chars per byte, no separators).
pub fn push_hex(out: &mut String, bytes: &[u8]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    out.reserve(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
}

/// Append `s` to `out` as the interior of a JSON string (no surrounding quotes), escaping
/// per RFC 8259.
pub fn json_escape(out: &mut String, s: &str) {
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
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
}

/// The human-readable name for a binder return `cmd` bindfetto reports as an error.
pub fn br_error_name(code: u32) -> &'static str {
    match code {
        BR_DEAD_REPLY => "BR_DEAD_REPLY",
        BR_FAILED_REPLY => "BR_FAILED_REPLY",
        BR_FROZEN_REPLY => "BR_FROZEN_REPLY",
        _ => "BR_ERROR",
    }
}

/// Human-readable cause for a binder failure errno (`return_error_param`). Covers the
/// causes seen in practice; unknown codes return `None` (the call site shows the number).
pub fn errno_reason(errno: i32) -> Option<&'static str> {
    Some(match errno {
        -28 => "target buffer full",     // ENOSPC
        -3 => "dead node",               // ESRCH
        -22 => "invalid transaction",    // EINVAL
        -1 => "operation not permitted", // EPERM
        -13 => "permission denied",      // EACCES (often SELinux)
        -9 => "bad file descriptor",     // EBADF
        -14 => "fault copying data",     // EFAULT
        -12 => "out of memory",          // ENOMEM
        -11 => "would block",            // EAGAIN
        -110 => "timed out",             // ETIMEDOUT
        _ => return None,
    })
}

/// Parse one kernel `failed_transaction_log` line into `(debug_id, errno)`. Each line is
/// `<id>: call from A:B to C:D context <ctx> … ret <return_error>/<param> l=<line>`; the
/// `param` after the slash is the concrete errno. Returns `None` for lines that don't
/// match (header lines, truncated entries).
pub fn parse_failed_tx_entry(line: &str) -> Option<(i32, i32)> {
    // The debug id is the leading `<id>:` token; a later `from A:B` also contains ':',
    // so split on the first one only.
    let (id_tok, rest) = line.split_once(':')?;
    let id = id_tok.trim().parse::<i32>().ok()?;
    // `ret <return_error>/<param>` — the param after the slash is the errno.
    let after = rest.split(" ret ").nth(1)?;
    let tok = after.split_whitespace().next()?;
    let (_, param) = tok.split_once('/')?;
    let errno = param.parse::<i32>().ok()?;
    Some((id, errno))
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- iface_key: the cross-component contract with the probe's WANTED map ---------

    fn utf16le(s: &str) -> Vec<u8> {
        s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect()
    }

    #[test]
    fn iface_key_is_zero_padded_utf16le() {
        let key = iface_key("android.os.IServiceManager");
        let want = utf16le("android.os.IServiceManager");
        assert_eq!(&key.0[..want.len()], &want[..]);
        // Everything past the descriptor is zero (matches the probe's zeroed buffer).
        assert!(key.0[want.len()..].iter().all(|&b| b == 0));
        assert_eq!(key.0.len(), MAX_IFACE_BYTES);
    }

    #[test]
    fn iface_key_round_trips_through_write_iface_bytes() {
        // What the consumer keys on must decode back to the same name the probe captured.
        let name = "android.hardware.automotive.vehicle.IVehicleCallback";
        let key = iface_key(name);
        let byte_len = utf16le(name).len();
        let mut out = String::new();
        assert!(write_iface_bytes(&mut out, &key.0, byte_len));
        assert_eq!(out, name);
    }

    #[test]
    fn iface_key_truncates_overlong_names_on_a_unit_boundary() {
        let long = "a".repeat(MAX_IFACE_BYTES); // 512 UTF-16 bytes, over the 256 cap
        let key = iface_key(&long);
        // Exactly MAX_IFACE_BYTES of 'a' units, no partial trailing unit.
        assert!(key.0.iter().all(|&b| b == b'a' as u8 || b == 0));
        assert_eq!(key.0[MAX_IFACE_BYTES - 2], b'a'); // last full unit low byte
    }

    // --- write_iface_bytes -----------------------------------------------------------

    #[test]
    fn write_iface_bytes_stops_at_nul_and_handles_empty() {
        let mut bytes = utf16le("IFoo");
        bytes.extend_from_slice(&utf16le("junk")); // after a NUL this must be ignored
        // Insert a NUL unit after "IFoo".
        let mut framed = utf16le("IFoo");
        framed.extend_from_slice(&[0, 0]);
        framed.extend_from_slice(&utf16le("junk"));
        let mut out = String::new();
        assert!(write_iface_bytes(&mut out, &framed, framed.len()));
        assert_eq!(out, "IFoo");

        // Zero length / out-of-range length write nothing and return false.
        let mut empty = String::new();
        assert!(!write_iface_bytes(&mut empty, &bytes, 0));
        assert!(!write_iface_bytes(&mut empty, &bytes, bytes.len() + 2));
        assert!(empty.is_empty());
    }

    #[test]
    fn write_iface_bytes_replaces_unpaired_surrogate() {
        // A lone high surrogate (0xD800) is invalid UTF-16 → U+FFFD.
        let bytes = [0x00, 0xD8];
        let mut out = String::new();
        assert!(write_iface_bytes(&mut out, &bytes, 2));
        assert_eq!(out, "\u{FFFD}");
    }

    // --- push_hex / json_escape ------------------------------------------------------

    #[test]
    fn push_hex_is_lowercase_two_per_byte() {
        let mut out = String::new();
        push_hex(&mut out, &[0x00, 0x0f, 0xa5, 0xff]);
        assert_eq!(out, "000fa5ff");
    }

    #[test]
    fn json_escape_covers_rfc8259_controls() {
        let mut out = String::new();
        json_escape(&mut out, "a\"b\\c\n\t\u{01}");
        assert_eq!(out, "a\\\"b\\\\c\\n\\t\\u0001");
    }

    // --- error / errno decode --------------------------------------------------------

    #[test]
    fn br_error_names() {
        assert_eq!(br_error_name(BR_DEAD_REPLY), "BR_DEAD_REPLY");
        assert_eq!(br_error_name(BR_FAILED_REPLY), "BR_FAILED_REPLY");
        assert_eq!(br_error_name(BR_FROZEN_REPLY), "BR_FROZEN_REPLY");
        assert_eq!(br_error_name(0x1234), "BR_ERROR");
    }

    #[test]
    fn errno_reasons() {
        assert_eq!(errno_reason(-28), Some("target buffer full"));
        assert_eq!(errno_reason(-3), Some("dead node"));
        assert_eq!(errno_reason(0), None);
        assert_eq!(errno_reason(-999), None);
    }

    // --- failed_transaction_log parsing ----------------------------------------------

    #[test]
    fn parses_failed_tx_line() {
        let line = "12345: call from 648:700 to 895:0 context binder node 4 handle 3 \
                    size 216:8 ret 29201/-28 l=42";
        assert_eq!(parse_failed_tx_entry(line), Some((12345, -28)));
    }

    #[test]
    fn parses_async_and_positive_ids() {
        let line = "7: async from 1:1 to 2:0 context binder ret 29189/-3 l=9";
        assert_eq!(parse_failed_tx_entry(line), Some((7, -3)));
    }

    #[test]
    fn rejects_lines_without_ret_field() {
        assert!(parse_failed_tx_entry("garbage without a colon").is_none());
        assert!(parse_failed_tx_entry("12: call from 1:2 to 3:4 no ret here").is_none());
        // A ret token without the `/param` slash.
        assert!(parse_failed_tx_entry("12: x ret 29201 l=1").is_none());
    }
}
