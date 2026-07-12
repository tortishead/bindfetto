//! DLT (AUTOSAR / COVESA) verbose message encoding — the on-wire bytes a DLT client
//! (DLT Viewer) reads from a network connection.
//!
//! Network format: no storage header (the viewer stamps its own reception header). A
//! message is: standard header (with ECU id + timestamp) + extended header + a single
//! verbose UTF-8 string argument. Pure/std-only so it can be unit-tested off-target.

// Standard header type (HTYP) flags.
const HTYP_UEH: u8 = 0x01; // extended header present
const HTYP_MSBF: u8 = 0x02; // payload is big-endian
const HTYP_WEID: u8 = 0x04; // ECU id present
const HTYP_WTMS: u8 = 0x10; // timestamp present
const HTYP_VERS1: u8 = 0x20; // protocol version 1 (bits 5-7)

// Extended header message-info (MSIN): verbose, message type LOG, level INFO.
const MSIN_VERBOSE: u8 = 0x01;
const MSIN_TYPE_LOG: u8 = 0x00 << 1;
const MSIN_LEVEL_INFO: u8 = 0x04 << 4;

// Verbose argument type-info: STRG (bit 9) + UTF-8 string coding (bit 15).
const TYPE_INFO_STRG_UTF8: u32 = 0x0000_0200 | 0x0000_8000;

// Fixed sizes of the two headers as emitted here.
const STD_HEADER_LEN: usize = 1 + 1 + 2 + 4 + 4; // htyp,mcnt,len,ecu,timestamp
const EXT_HEADER_LEN: usize = 1 + 1 + 4 + 4; // msin,noar,apid,ctid

/// Encode one verbose DLT log message carrying `text` (a single UTF-8 string
/// argument) into `out`. `ts_tenths_ms` is the DLT timestamp in units of 0.1 ms.
pub fn encode(
    out: &mut Vec<u8>,
    counter: u8,
    ts_tenths_ms: u32,
    ecu: &[u8; 4],
    apid: &[u8; 4],
    ctid: &[u8; 4],
    text: &str,
) {
    out.clear();

    // Verbose string payload is `type-info(4) | length(2) | bytes | NUL`, where the
    // length counts the trailing NUL.
    let payload_str_len = text.len() + 1;
    let payload_len = 4 + 2 + payload_str_len;
    let total = (STD_HEADER_LEN + EXT_HEADER_LEN + payload_len) as u16;

    // Standard header.
    out.push(HTYP_UEH | HTYP_MSBF | HTYP_WEID | HTYP_WTMS | HTYP_VERS1);
    out.push(counter);
    out.extend_from_slice(&total.to_be_bytes()); // LEN is always big-endian
    out.extend_from_slice(ecu);
    out.extend_from_slice(&ts_tenths_ms.to_be_bytes());

    // Extended header.
    out.push(MSIN_VERBOSE | MSIN_TYPE_LOG | MSIN_LEVEL_INFO);
    out.push(1); // number of arguments
    out.extend_from_slice(apid);
    out.extend_from_slice(ctid);

    // Payload: one UTF-8 string argument (big-endian per HTYP_MSBF).
    out.extend_from_slice(&TYPE_INFO_STRG_UTF8.to_be_bytes());
    out.extend_from_slice(&(payload_str_len as u16).to_be_bytes());
    out.extend_from_slice(text.as_bytes());
    out.push(0); // NUL terminator
}

/// Right-pad/truncate `s` to a 4-byte DLT id (ECU/App/Context).
pub fn id4(s: &str) -> [u8; 4] {
    let mut id = [0u8; 4];
    let b = s.as_bytes();
    let n = b.len().min(4);
    id[..n].copy_from_slice(&b[..n]);
    id
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id4_pads_and_truncates() {
        assert_eq!(id4("BFTO"), *b"BFTO");
        assert_eq!(id4("AB"), [b'A', b'B', 0, 0]);
        assert_eq!(id4("TOOLONG"), *b"TOOL");
        assert_eq!(id4(""), [0, 0, 0, 0]);
    }

    #[test]
    fn encode_round_trips_header_and_payload() {
        let mut out = Vec::new();
        encode(&mut out, 0x2a, 0x0001_0000, b"BFTO", b"BIND", b"BIND", "hi");

        // HTYP: UEH|MSBF|WEID|WTMS|VERS1 = 0x37.
        assert_eq!(out[0], 0x37);
        assert_eq!(out[1], 0x2a); // message counter

        // LEN (big-endian) equals the whole message.
        let len = u16::from_be_bytes([out[2], out[3]]) as usize;
        assert_eq!(len, out.len());
        // std(12) + ext(10) + payload(type-info 4 + len 2 + "hi" 2 + NUL 1) = 31.
        assert_eq!(len, 31);

        assert_eq!(&out[4..8], b"BFTO"); // ECU id
        assert_eq!(u32::from_be_bytes([out[8], out[9], out[10], out[11]]), 0x0001_0000);

        // Extended header: MSIN verbose+LOG+INFO, then noar=1, apid, ctid.
        assert_eq!(out[12], MSIN_VERBOSE | MSIN_TYPE_LOG | MSIN_LEVEL_INFO);
        assert_eq!(out[13], 1);
        assert_eq!(&out[14..18], b"BIND");
        assert_eq!(&out[18..22], b"BIND");

        // Payload: STRG|UTF8 type-info, string length incl. NUL, "hi\0".
        assert_eq!(u32::from_be_bytes([out[22], out[23], out[24], out[25]]), TYPE_INFO_STRG_UTF8);
        assert_eq!(u16::from_be_bytes([out[26], out[27]]), 3); // "hi" + NUL
        assert_eq!(&out[28..], b"hi\0");
    }

    #[test]
    fn encode_clears_prior_contents() {
        let mut out = vec![0xff; 100];
        encode(&mut out, 0, 0, b"ECU1", b"AP01", b"CT01", "x");
        assert_eq!(u16::from_be_bytes([out[2], out[3]]) as usize, out.len());
    }
}
