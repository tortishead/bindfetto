//! Structured parsing of a bindfetto core line into typed fields.
//!
//! [`Decoder::decode_line`](crate::Decoder::decode_line) rewrites a line in place
//! without needing the full structure; [`Record::parse`] is the richer view, for
//! tools that want the individual fields (a `--json` mode, filtering, analysis).

/// The method/label portion of a transaction line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Label<'a> {
    /// An AIDL call carrying an interface descriptor: `interface.[code:N]`.
    Aidl { iface: &'a str, code: u32 },
    /// A reply (carries no descriptor by design): `<reply code:N>`.
    Reply { code: u32 },
    /// A transaction with no AIDL interface token (HIDL/hwbinder or a special
    /// transaction): `<non-aidl code:N>`.
    NonAidl { code: u32 },
}

/// A parsed transaction line — the sink-independent core:
/// `src (pid) -> dst (pid): <label>, <size>B[ oneway]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record<'a> {
    pub src: &'a str,
    pub src_pid: u32,
    pub dst: &'a str,
    pub dst_pid: u32,
    pub label: Label<'a>,
    pub size: u32,
    pub oneway: bool,
}

impl<'a> Record<'a> {
    /// Parse a bindfetto core line. Any prefix (a console timestamp, the `BINDFETTO`
    /// marker, logcat metadata) must be stripped by the caller first. Returns `None`
    /// if the line doesn't match the expected shape.
    pub fn parse(core: &'a str) -> Option<Self> {
        let core = core.trim();
        let (core, oneway) = match core.strip_suffix(" oneway") {
            Some(rest) => (rest, true),
            None => (core, false),
        };
        // Trailing ", <size>B".
        let comma = core.rfind(", ")?;
        let (head, size_part) = (&core[..comma], &core[comma + 2..]);
        let size = size_part.strip_suffix('B')?.parse().ok()?;
        // "): " separates the "src (pid) -> dst (pid)" addressing from the label.
        // Anchored on "): " so the ':' inside "[code:N]" is never mistaken for it.
        let sep = head.find("): ")?;
        let addr = &head[..sep + 1]; // include the closing ')'
        let label = parse_label(&head[sep + 3..])?;
        let arrow = addr.find(" -> ")?;
        let (src, src_pid) = parse_endpoint(&addr[..arrow])?;
        let (dst, dst_pid) = parse_endpoint(&addr[arrow + 4..])?;
        Some(Record {
            src,
            src_pid,
            dst,
            dst_pid,
            label,
            size,
            oneway,
        })
    }
}

/// `name (pid)` → `(name, pid)`.
fn parse_endpoint(s: &str) -> Option<(&str, u32)> {
    let open = s.rfind(" (")?;
    let name = &s[..open];
    let pid = s[open + 2..].strip_suffix(')')?.parse().ok()?;
    Some((name, pid))
}

fn parse_label(s: &str) -> Option<Label<'_>> {
    if let Some(rest) = s.strip_prefix("<reply code:") {
        return Some(Label::Reply {
            code: rest.strip_suffix('>')?.parse().ok()?,
        });
    }
    if let Some(rest) = s.strip_prefix("<non-aidl code:") {
        return Some(Label::NonAidl {
            code: rest.strip_suffix('>')?.parse().ok()?,
        });
    }
    // "<iface>.[code:<N>]"
    let mark = s.find(".[code:")?;
    let iface = &s[..mark];
    if iface.is_empty() {
        return None;
    }
    let code = s[mark + ".[code:".len()..].strip_suffix(']')?.parse().ok()?;
    Some(Label::Aidl { iface, code })
}
