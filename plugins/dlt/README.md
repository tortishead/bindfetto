# Bindfetto DLT Viewer plugin

A [DLT Viewer](https://github.com/COVESA/dlt-viewer) **decoder plugin** that rewrites
bindfetto Binder transaction codes to AIDL method names inline, e.g.

```text
… android.app.IActivityManager.[code:7] …   →   … android.app.IActivityManager.startActivity …
```

It is a thin C++/Qt shell (`QDLTPluginDecoderInterface`) over the Rust
[`decode/`](../../decode) core — all parsing and catalog lookup live in the core; this
plugin only bridges `QDltMsg` to the core's C ABI. On automotive targets the OEM
already pipes logcat into DLT, so no bridge of ours is needed: bindfetto's logcat lines
arrive as DLT messages carrying the `BINDFETTO` marker, which is how the plugin
recognizes them (`isMsg`).

## How it works

- **`isMsg`** — true when the message payload contains `BINDFETTO`.
- **`decodeMsg`** — passes the payload text through `bf_decode_line`, then replaces the
  message with a single UTF-8 string argument holding the decoded line.
- **`loadConfig(path)`** — the plugin's config (set in the DLT Viewer plugin manager)
  is the **AIDL catalog** (Track B1 output): either a single JSON file, or a **folder**,
  in which case every `*.json` under it is merged into one catalog (via `QJsonObject`;
  later files win per code). Without a catalog loaded, `isMsg` returns false and
  messages pass through untouched.

## Getting bindfetto into DLT

On automotive targets the OEM usually bridges logcat into DLT, so bindfetto's logcat
lines arrive as DLT messages for this plugin to decode. Where that bridge is **absent**,
run the bindfetto binary with `--dlt-serve [port]` (default 3490): bindfetto becomes a
DLT TCP endpoint and streams each transaction as a verbose DLT message — no libdlt, no
dlt-daemon. Connect DLT Viewer to it as a TCP ECU:

```sh
adb forward tcp:3490 tcp:3490
# then in DLT Viewer add a TCP ECU at localhost:3490
```

The lines show up in DLT Viewer's live trace and this plugin decodes them the same way.

## Build

DLT Viewer plugins are native C++/Qt shared libraries and must be built against the
same Qt major version and compiler ABI as your dlt-viewer. You need the dlt-viewer
`qdlt` SDK (its export headers + `libqdlt`).

1. Build the Rust core static library (produces `libbindfetto_decode.a` and the C
   header):

   ```sh
   (cd ../../decode && cargo build --release)
   ```

   For a device/target build, cross-compile the core for that triple and point
   `BINDFETTO_DECODE_LIB` at the resulting `.a`.

2. Configure and build the plugin, pointing CMake at your dlt-viewer SDK:

   ```sh
   cmake -B build \
     -DDLT_VIEWER_QDLT_INCLUDE_DIR=/path/to/dlt-viewer/qdlt \
     -DDLT_VIEWER_QDLT_LIB=/path/to/dlt-viewer/build/lib/libqdlt.so
   cmake --build build
   ```

   (Alternatively, drop this directory into the dlt-viewer source tree under `plugin/`
   and add it to that build, which resolves the qdlt headers/lib automatically.)

3. Copy the built `libbindfettodecoderplugin.{so,dylib,dll}` into DLT Viewer's plugins
   directory (or add its folder in *Settings → Plugins*). Enable the plugin and set its
   config file to your `catalog.json`.

## Notes

- The CMake links the core's **static** `libbindfetto_decode.a`, so the built plugin
  embeds the decoder and has no external `bindfetto_decode` dependency. On macOS the
  Rust runtime pulls in the CoreFoundation/Security frameworks (handled in CMake).
- `pluginInterfaceVersion()` returns the SDK's `PLUGIN_INTERFACE_VERSION`; the plugin
  must be rebuilt if the dlt-viewer plugin interface version changes.
- The single `BfDecoder` is immutable after `loadConfig` and only borrowed by
  `decodeMsg`, so it is safe if the viewer decodes on multiple threads.

## Verified

Built and runtime-tested on macOS (arm64) against a source build of dlt-viewer with
Qt 6.11: the plugin loads via `QPluginLoader`, casts to `QDLTPluginInterface` /
`QDLTPluginDecoderInterface`, and `decodeMsg` rewrites `interface.[code:N]` to the
catalog method name (e.g. `android.app.IActivityManager.[code:7]` →
`android.app.IActivityManager.startActivity`), leaving non-bindfetto messages untouched.
dlt-viewer runs on macOS but is not *officially* supported there; Linux/Windows use the
same CMake.
