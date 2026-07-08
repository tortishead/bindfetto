//! The AIDL catalog: the shared contract between the (dumb) runtime logs and the
//! (smart) offline decoders.

use std::collections::HashMap;

/// A precompiled AIDL catalog: interface descriptor → (transaction code → method).
///
/// Built by the Track B1 Python catalog builder from a folder of AIDL, reading the
/// real `TRANSACTION_* = FIRST_CALL_TRANSACTION + N` constants so explicit `= N` ids
/// are honored. JSON shape:
///
/// ```json
/// {
///   "android.app.IActivityManager": { "1": "getTasks", "7": "startActivity" }
/// }
/// ```
pub struct Catalog {
    interfaces: HashMap<String, HashMap<u32, String>>,
}

impl Catalog {
    /// Parse a catalog from its JSON representation.
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        let interfaces = serde_json::from_str(json)?;
        Ok(Self { interfaces })
    }

    /// An empty catalog. Special transactions still resolve (they're interface-agnostic).
    pub fn empty() -> Self {
        Self {
            interfaces: HashMap::new(),
        }
    }

    /// Number of interfaces covered.
    pub fn len(&self) -> usize {
        self.interfaces.len()
    }

    /// True if the catalog covers no interfaces.
    pub fn is_empty(&self) -> bool {
        self.interfaces.is_empty()
    }

    /// Method name for a normal AIDL call, if the `interface` and `code` are known.
    /// Does not cover special transactions — see [`special_transaction`].
    pub fn method(&self, iface: &str, code: u32) -> Option<&str> {
        self.interfaces.get(iface)?.get(&code).map(String::as_str)
    }
}

/// Well-known non-AIDL transaction codes shared by every Binder object (`IBinder`),
/// packed from four chars via `B_PACK_CHARS`. These live outside the AIDL call range
/// (`FIRST_CALL_TRANSACTION..=LAST_CALL_TRANSACTION`) and are the same for every
/// interface, so they resolve without a catalog.
pub fn special_transaction(code: u32) -> Option<&'static str> {
    Some(match code {
        0x5f4e5446 => "INTERFACE", // _NTF  getInterfaceDescriptor
        0x5f444d50 => "DUMP",      // _DMP
        0x5f504e47 => "PING",      // _PNG
        0x5f535052 => "SYSPROPS",  // _SPR
        0x5f434d44 => "SHELL_CMD", // _CMD
        _ => return None,
    })
}
