//! End-to-end tests for the decode core against a small fixture catalog.

use bindfetto_decode::{Decoder, Label, Record};

fn decoder() -> Decoder {
    Decoder::from_catalog_json(include_str!("fixtures/catalog.json")).expect("catalog parses")
}

#[test]
fn rewrites_known_method() {
    let d = decoder();
    let line = "com.example.app (1234) -> system_server (5678): android.app.IActivityManager.[code:7], 512B";
    assert_eq!(
        d.decode_line(line),
        "com.example.app (1234) -> system_server (5678): android.app.IActivityManager.startActivity, 512B"
    );
}

#[test]
fn keeps_unknown_code() {
    let d = decoder();
    let line = "a (1) -> b (2): android.app.IActivityManager.[code:999], 8B";
    assert_eq!(d.decode_line(line), line);
}

#[test]
fn keeps_unknown_interface() {
    let d = decoder();
    let line = "a (1) -> b (2): com.unknown.IThing.[code:1], 8B";
    assert_eq!(d.decode_line(line), line);
}

#[test]
fn rewrite_is_prefix_agnostic() {
    let d = decoder();
    // Console timestamp prefix + trailing oneway.
    let ts = "18:12:09.861 system_server (658) -> com.android.systemui (1128): android.app.ITaskStackListener.[code:12], 2808B oneway";
    assert_eq!(
        d.decode_line(ts),
        "18:12:09.861 system_server (658) -> com.android.systemui (1128): android.app.ITaskStackListener.onTaskMovedToFront, 2808B oneway"
    );
    // BINDFETTO marker prefix (logcat/DLT form).
    let marked = "BINDFETTO com.example.app (1) -> system_server (2): android.app.IActivityManager.[code:1], 4B";
    assert_eq!(
        d.decode_line(marked),
        "BINDFETTO com.example.app (1) -> system_server (2): android.app.IActivityManager.getTasks, 4B"
    );
}

#[test]
fn resolves_special_transaction() {
    let d = decoder();
    // 0x5f504e47 = 1599098439 = PING, interface-agnostic (resolves without a catalog).
    let line = "a (1) -> b (2): android.os.IServiceManager.[code:1599098439], 0B";
    assert_eq!(
        d.decode_line(line),
        "a (1) -> b (2): android.os.IServiceManager.PING, 0B"
    );
}

#[test]
fn leaves_reply_and_nonaidl_untouched() {
    let d = decoder();
    // Neither carries a ".[code:" token, so there's nothing to rewrite.
    let reply = "a (1) -> b (2): <reply code:0>, 4B";
    let nonaidl = "a (1) -> b (2): <non-aidl code:1599098439>, 0B";
    assert_eq!(d.decode_line(reply), reply);
    assert_eq!(d.decode_line(nonaidl), nonaidl);
}

#[test]
fn borrows_when_unchanged() {
    let d = decoder();
    let line = "nothing to decode here";
    assert!(matches!(d.decode_line(line), std::borrow::Cow::Borrowed(_)));
}

#[test]
fn empty_catalog_still_resolves_special() {
    let d = Decoder::from_catalog_json("{}").unwrap();
    assert_eq!(d.method("whatever.IThing", 0x5f444d50), Some("DUMP"));
    assert_eq!(d.method("whatever.IThing", 7), None);
}

#[test]
fn parses_record() {
    let core = "com.example.app (1234) -> system_server (5678): android.app.IActivityManager.[code:7], 512B";
    let rec = Record::parse(core).expect("parses");
    assert_eq!(rec.src, "com.example.app");
    assert_eq!(rec.src_pid, 1234);
    assert_eq!(rec.dst, "system_server");
    assert_eq!(rec.dst_pid, 5678);
    assert_eq!(rec.size, 512);
    assert!(!rec.oneway);
    assert_eq!(
        rec.label,
        Label::Aidl {
            iface: "android.app.IActivityManager",
            code: 7
        }
    );
}

#[test]
fn parses_oneway_reply_and_nonaidl() {
    let rec = Record::parse("a (1) -> b (2): <reply code:0>, 4B").unwrap();
    assert_eq!(rec.label, Label::Reply { code: 0 });
    assert!(!rec.oneway);

    let rec = Record::parse("x (1) -> y (2): x.IY.[code:3], 8B oneway").unwrap();
    assert!(rec.oneway);
    assert_eq!(rec.label, Label::Aidl { iface: "x.IY", code: 3 });

    let rec = Record::parse("x (1) -> y (2): <non-aidl code:42>, 0B").unwrap();
    assert_eq!(rec.label, Label::NonAidl { code: 42 });
}

// --- M6 parcel argument decoding ------------------------------------------------

/// Append a `String16` (int32 len, chars + null, padded to 4) to a parcel body.
fn write_string16(out: &mut Vec<u8>, s: &str) {
    let units: Vec<u16> = s.encode_utf16().collect();
    out.extend_from_slice(&(units.len() as u32).to_le_bytes());
    for u in &units {
        out.extend_from_slice(&u.to_le_bytes());
    }
    out.extend_from_slice(&0u16.to_le_bytes()); // null terminator
    while out.len() % 4 != 0 {
        out.push(0);
    }
}

/// Build a full parcel: the interface-token header (matching the probe's framing) plus
/// a marshalled argument `body`.
fn build_parcel(descriptor: &str, body: &[u8]) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&0u32.to_le_bytes()); // strict-mode policy
    p.extend_from_slice(&0u32.to_le_bytes()); // work-source
    p.extend_from_slice(&0x5359_5354u32.to_le_bytes()); // 'SYST'
    let mut header_str = Vec::new();
    write_string16(&mut header_str, descriptor);
    // write_string16 wrote the u32 length first; that lives at offset 12 as the token
    // length, followed by the chars — exactly what seek_args expects.
    p.extend_from_slice(&header_str);
    p.extend_from_slice(body);
    p
}

fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::new();
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[test]
fn renders_parcel_arguments() {
    let d = decoder();
    let mut body = Vec::new();
    write_string16(&mut body, "hi"); // tag = "hi"
    body.extend_from_slice(&7i32.to_le_bytes()); // flags = 7
    let parcel = build_parcel("test.IFoo", &body);
    let line = format!(
        "a (1) -> b (2): test.IFoo.[code:1], {}B parcel={}/{}:{}",
        parcel.len(),
        parcel.len(),
        parcel.len(),
        to_hex(&parcel),
    );
    assert_eq!(
        d.decode_line(&line),
        format!(
            "a (1) -> b (2): test.IFoo.doThing(tag=\"hi\", flags=7), {}B",
            parcel.len()
        )
    );
}

#[test]
fn marks_truncated_parcel() {
    let d = decoder();
    let mut body = Vec::new();
    write_string16(&mut body, "hi");
    // flags int omitted → the reader runs out mid-signature.
    let parcel = build_parcel("test.IFoo", &body);
    let line = format!(
        "a (1) -> b (2): test.IFoo.[code:1], 999B parcel={}/999:{}",
        parcel.len(),
        to_hex(&parcel),
    );
    assert_eq!(
        d.decode_line(&line),
        "a (1) -> b (2): test.IFoo.doThing(tag=\"hi\", …(truncated)), 999B"
    );
}

#[test]
fn stops_at_unparsable_type() {
    let d = decoder();
    // code 2 = bind(token: IBinder) — no fixed layout, so decoding stops immediately.
    let parcel = build_parcel("test.IFoo", &[]);
    let line = format!(
        "a (1) -> b (2): test.IFoo.[code:2], 8B parcel={}/8:{}",
        parcel.len(),
        to_hex(&parcel),
    );
    assert_eq!(
        d.decode_line(&line),
        "a (1) -> b (2): test.IFoo.bind(token=<IBinder>, …(unparsed)), 8B"
    );
}

#[test]
fn parcel_token_kept_when_method_unknown() {
    let d = decoder();
    // Unknown code: no method, so the parcel token passes through untouched.
    let line = "a (1) -> b (2): test.IFoo.[code:99], 8B parcel=4/8:deadbeef";
    assert_eq!(d.decode_line(line), line);
}

#[test]
fn parses_endpoint_paths_with_slashes() {
    let core =
        "/system/bin/surfaceflinger (421) -> system_server (658): android.gui.IFoo.[code:3], 88B";
    let rec = Record::parse(core).expect("parses");
    assert_eq!(rec.src, "/system/bin/surfaceflinger");
    assert_eq!(rec.src_pid, 421);
}
