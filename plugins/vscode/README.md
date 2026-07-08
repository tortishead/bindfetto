# Bindfetto Decode — VS Code extension

Resolves bindfetto Binder transaction codes to AIDL method names inside VS Code, e.g.

```text
… android.app.IActivityManager.[code:7] …   →   … android.app.IActivityManager.startActivity …
```

It is a thin adapter over the Rust [`decode/`](../../decode) core, compiled to
**WebAssembly** — the same decode logic as the CLI and the DLT Viewer plugin, written
once. WASM keeps distribution simple: one `.wasm` in the package, no per-platform
native builds.

## Architecture

```
decode/ (Rust core) ──► plugins/vscode/wasm (cdylib, wasm32) ──► media/*.wasm
                                                                      │
                                        src/decoder.ts (JS loader) ───┘
                                        src/extension.ts (command + config)
```

- `wasm/` — a small Rust crate that re-exports the core's decoder C ABI and adds a
  byte allocator (`bf_alloc`/`bf_free`) so JS can pass UTF-8 strings across the wasm
  boundary.
- `src/decoder.ts` — instantiates the module and marshals strings through linear
  memory; exposes `BindfettoDecoder.load(wasmBytes, catalogJson)` → `decodeLine(...)`.
- `src/extension.ts` — one command (**Bindfetto: Decode Active Editor**) that opens a
  decoded copy of the active document beside it, plus the `bindfetto.catalogPath`
  setting.

## Build

Needs the Rust `wasm32-unknown-unknown` target and a modern Node.

```sh
rustup target add wasm32-unknown-unknown
npm install
npm run build:wasm   # cargo build (wasm32) → copies media/bindfetto_decode_wasm.wasm
npm run compile      # tsc → dist/
```

`npm run smoke` runs a standalone Node check of the wasm decoder (no VS Code needed).
Press F5 in VS Code to launch the Extension Development Host.

## Use

1. Set `bindfetto.catalogPath` to your AIDL catalog JSON (Track B1 output).
2. Open a bindfetto log (or paste `adb logcat -s bindfetto` output).
3. Run **Bindfetto: Decode Active Editor** — a decoded copy opens beside it.

## Status

Verified on macOS with Node 26: the wasm builds and exports the expected symbols,
`tsc` typechecks clean, `npm run smoke` passes, and the compiled `dist/decoder.js`
decodes real lines end-to-end through the wasm core (e.g.
`android.app.ITaskStackListener.[code:12]` → `…onTaskMovedToFront`). The in-editor
command wiring itself is exercised by launching the Extension Development Host (F5).
