# Roadmap

Design lives in [SPEC.md](./SPEC.md). This is the build order.

## Track A — on-device runtime (`runtime/`)

Vertical slices; each one runs on the AVD before the next starts.

- **M1 — bare pipeline.** ✅ **Done.** Attach to `binder:binder_transaction`, push
  `{src_pid, dst_pid, code, flags, size}` through the ring buffer, print to
  console. Verified live on an arm64 AVD — captures real binder traffic with
  correct pids/code/flags and oneway detection.
- **M2 — process names.** ✅ **Done.** Resolve `/proc/<pid>/cmdline` with a
  pid→name cache; emit `name (pid) -> name (pid)`.
- **M3 — interface descriptor + size.** ✅ **Done.** A kprobe on
  `binder_transaction()` reads `data_size` and the parcel buffer; the consumer
  UTF-16-decodes the interface descriptor (validated by the `'SYST'` token magic).
  Replies show `<reply>`, HIDL/hwbinder & special transactions show `<non-aidl>`.
  Verified live on the AVD (automotive AIDL + a HIDL bluetooth call).
- **M4 — in-kernel filter.** Hash descriptor bytes in the probe; match against a
  BPF map of wanted hashes to drop before the ring buffer.
- **M5 — errors + sinks + CLI.** In progress.
  - ✅ Console sink with wall-clock timestamp.
  - ✅ Logcat sink (`--sink console|logcat|both`), tag `bindfetto` + `BF1` marker.
  - ⏳ File / JSONL sink.
  - ⏳ Second attach point for `BR_FAILED_REPLY`/`BR_DEAD_REPLY` (toggleable).
  - ⏳ Full CLI (interface filter, `--include-replies`, error toggle).

## Track B — offline decode

- **B1 — catalog builder** (`catalog/`, Python): folder of AIDL → JSON catalog via
  generated stubs / `aidl --dumpapi`; handle explicit `= N` ids and special
  transactions.
- **B2 — shared decoder core + `bindfetto-decode` CLI**: line parse + catalog
  lookup → method name.
- **B3 — viewer plugins**: VS Code first, DLT Viewer for the automotive audience.

## Track C — control app (`app/`, Kotlin)

- **C1 — control channel**: unix socket + command protocol (shared with the CLI).
- **C2 — app**: deploy binary (signature permission), start/stop, interface
  filter, error toggle.

Tracks B and C start once Track A produces stable output (≈after M3).
