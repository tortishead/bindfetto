# bindfetto-decode

The offline **decode core** (Track B2): resolves the raw Binder transaction codes in
bindfetto logs to human-readable method names, using a precompiled AIDL catalog.

The on-device runtime deliberately emits the *raw* code — decoding on the hot path
would be expensive and would pin the logs to one catalog version. This crate does the
lookup after the fact, so the same captured logs can be re-decoded against any catalog.

```text
… android.app.IActivityManager.[code:7], 512B      →      … android.app.IActivityManager.startActivity, 512B
```

## Layout

This is the plugin-agnostic core the SPEC calls for. The CLI, the DLT Viewer plugin
(via a C ABI), and the VS Code extension (via WASM) are all thin adapters over it.

| Item | Role |
|---|---|
| `Decoder` / `Catalog` (`lib.rs`, `catalog.rs`) | Catalog load + `(interface, code) → method`. |
| `Decoder::decode_line` | Rewrite `interface.[code:N]` tokens in a line, in place. Prefix-agnostic (works with a console timestamp, the `BINDFETTO` marker, or logcat/DLT wrapping) and leaves unknown codes and non-bindfetto lines untouched. |
| `Record` / `Label` (`parse.rs`) | Structured field-level parse, for tools that want `src/dst/pid/size/oneway`. |
| `bindfetto-decode` (`main.rs`) | stdin→stdout / file CLI adapter. |

Separate from `runtime/` (which only cross-compiles to Android): this builds on the
host, like the plugins that will embed it.

## Usage

```sh
adb logcat -s bindfetto | bindfetto-decode --catalog catalog.json
# or decode a captured file
bindfetto-decode --catalog catalog.json capture.log
```

The catalog JSON is `interface → { code → method }` (produced by the Track B1 Python
builder, not yet built):

```json
{ "android.app.IActivityManager": { "1": "getTasks", "7": "startActivity" } }
```

Special interface-agnostic transactions (`PING`/`DUMP`/`INTERFACE`/`SYSPROPS`/
`SHELL_CMD`) resolve without a catalog entry.

## Test

```sh
cargo test
```
