# bindfetto runtime (eBPF probe + userspace consumer)

The on-device half of bindfetto: an [`aya`](https://aya-rs.dev) eBPF probe that
captures Binder transactions and a Rust userspace consumer that drains them.

> **Build status.** The whole runtime compiles and runs: the **eBPF probe** for
> `bpfel-unknown-none` and the **userspace consumer** for `aarch64-linux-android`
> (with the eBPF object embedded via `aya-build`), against aya 0.13.1 / aya-ebpf
> 0.1.1 with NDK r26+ (any recent NDK). **Verified live on an arm64 AVD** (milestones M1–M5): real
> binder capture, interface descriptors, in-kernel filtering, error events, and every
> sink. `aya` is Linux-only, so the consumer only builds for the Android target, not
> the macOS host. The tracepoint field offsets are set for the dev AVD kernel —
> re-check them on a different device (see below).

## Layout

| Crate | Role |
|---|---|
| `bindfetto-common` | Shared `#[repr(C)]` `TxEvent` — the ring-buffer wire contract. |
| `bindfetto-ebpf` | `no_std` eBPF probe; built for `bpfel-unknown-none`. |
| `bindfetto` | Userspace consumer; loads the probe, drains the ring buffer, prints. |

## Prerequisites

```sh
# Rust (nightly is pinned via rust-toolchain.toml for the eBPF build)
curl https://sh.rustup.rs -sSf | sh
rustup component add rust-src

# eBPF linker + Android cross-compile target
cargo install bpf-linker
rustup target add aarch64-linux-android

# Android SDK/NDK for the cross-linker + adb + emulator (arm64 system image)
```

## Dev target: Android emulator (AVD)

Use an **arm64** system image (runs natively on Apple silicon; recent images ship
kernel 5.10/5.15 with BTF + `RingBuf`). eBPF loading needs root and a permissive
SELinux domain:

```sh
adb root
adb shell setenforce 0     # BPF load is SELinux-gated; permissive for dev

# Confirm the tracepoint exists and CHECK THE FIELD OFFSETS used in the probe:
adb shell cat /sys/kernel/tracing/events/binder/binder_transaction/format
```

> The offsets in `bindfetto-ebpf/src/main.rs` (`OFF_TO_PROC`, `OFF_CODE`,
> `OFF_FLAGS`) are set for the dev AVD kernel — confirm them against that `format`
> output when moving to a different device/kernel.

### Kernel must have kprobes (`CONFIG_KPROBES`)

The probe attaches a **kprobe** on `binder_transaction()` to read the parcel — that's
where the interface descriptor and the M6 payload come from. Without kprobe support the
kprobe won't attach and **bindfetto exits at startup** (`attach kprobe binder_transaction`).
Check before running:

```sh
# Preferred: the kernel config, if exposed (CONFIG_IKCONFIG_PROC).
adb shell 'zcat /proc/config.gz 2>/dev/null | grep -E "CONFIG_KPROBES|CONFIG_KPROBE_EVENTS"'
# Expect: CONFIG_KPROBES=y  and  CONFIG_KPROBE_EVENTS=y

# Fallback when /proc/config.gz is absent: the tracefs kprobe interface must exist.
adb shell 'ls /sys/kernel/tracing/kprobe_events && echo kprobes-ok'
```

If neither is present the kernel was built without kprobes; the tracepoint-only data
(pids / code / size / errors) is still reachable in principle, but this build requires the
kprobe and won't start — rebuild the kernel with `CONFIG_KPROBES=y` (+ `CONFIG_KPROBE_EVENTS=y`),
or use a device/AVD whose kernel has them (the arm64 AVD images do).

### Debugging pid→name resolution

Names come from `/proc/<pid>/cmdline` (then `comm`) resolved in userspace, so a
short-lived process can exit before it's read and show as `pid:<n>`. To see why a given
pid stays unresolved, set `BINDFETTO_DEBUG=1` — the resolver traces each attempt to
stderr, and for a failure reports whether `/proc/<pid>` still exists:

```sh
adb shell 'BINDFETTO_DEBUG=1 /data/local/tmp/bindfetto --sink none' 2>&1 | grep '\[names\]'
# [names] pid 26824: UNRESOLVED (/proc/26824 exists: false)   -> exit race (process gone)
# [names] pid 32414: cmdline read failed: <errno>             -> live but unreadable
```

`exists: false` is the unavoidable exit race. `exists: true` (or a read error on a live
pid) points at a permissions / pid-namespace / procfs issue worth reporting.

## Build & run (Milestone 1)

```sh
# Cross-compile the consumer (embeds the eBPF object via build.rs)
cargo build --release --target aarch64-linux-android

# Push and run on the emulator
adb push target/aarch64-linux-android/release/bindfetto /data/local/tmp/
adb shell /data/local/tmp/bindfetto      # run as root
```

Expected output (M1–M3), one line per transaction:

```
com.android.car (11428) -> ...vehicle@V1-emulator-service (11410): android.hardware.automotive.vehicle.IVehicle.[code:3], 228B
...vehicle@V1-emulator-service (11410) -> com.android.car (11428): <reply code:0>, 4B
...bluetooth@1.1-service.btlinux (12743) -> hwservicemanager (154): <non-aidl code:3>, 204B
```

Process names come from `/proc/<pid>/cmdline`; the interface descriptor is decoded
from the parcel (AIDL `'SYST'` token). Replies and HIDL/hwbinder transactions are
labeled `<reply>` / `<non-aidl>`. The method name (from `[code:N]`) is resolved
offline against the AIDL catalog — a later milestone.

## Options

| Flag | Effect |
|---|---|
| `--sink console\|logcat\|both\|none` | Human-readable line sink (default `console`; `none` = quiet, file/DLT only). |
| `--jsonl <path>` | Also write one JSON object per transaction to `<path>`. Composes with any sink. |
| `--dlt-serve [port]` | Be a DLT TCP server (default 3490); DLT Viewer connects as a TCP ECU for live trace. The server binds *on the device* — from the host run `adb forward tcp:3490 tcp:3490`, then point DLT Viewer's TCP ECU at `localhost:3490`. |
| `--iface <name>` | **In-kernel** interface filter: keep only these descriptors, dropping the rest in the probe before the ring buffer. Repeatable and comma-separated (`--iface a.b.IFoo --iface a.c.IBar,a.c.IBaz`). Match is exact (full descriptor), so `IVehicle` does not match `IVehicleCallback`. While a filter is active, transactions with no interface token (replies already excluded, special/native transactions) also drop. |
| `--errors [on\|off]` | Capture transaction errors (`BR_FAILED_REPLY`/`BR_DEAD_REPLY`/`BR_FROZEN_REPLY`) via a second `binder:binder_return` attach point, off by default. Each error names the failing source → target, interface and method, and (best-effort) the concrete errno decoded from the kernel `failed_transaction_log`. Toggleable live over the control channel. |
| `--parcel [on\|off]` | Capture the raw parcel payload (method arguments), off by default. Only honored while an interface filter is active (`--iface`), so capture stays bounded to the selected interfaces. The device emits raw bytes as a `parcel=<captured>/<total>:<hex>` token / a `parcel` field in JSONL; arguments are decoded **offline** (see below). Toggleable live over the control channel (`PARCEL on\|off`). |
| `--parcel-max <bytes>` | Per-transaction cap on captured payload (default 256, max 30720). Bigger catches large parcels at more ring/CPU cost; the ring only pays for the bytes actually captured. Retunable live (`PARCEL max <n>`). |
| `--include-replies` | Keep normal (successful) replies, which are otherwise dropped in the probe before the ring buffer. |
| `--control [port]` | Control channel for the Track C app (default 3491): a line TCP server driving the runtime live. Commands: `STATUS`; `START`/`STOP` (capture toggle); `SINK console\|logcat\|both\|none`; `DLT on\|off`; `ERRORS on\|off` (error capture); `PARCEL on\|off` / `PARCEL max <bytes>` (parcel payload capture + cap); `TRACK on\|off` (interface discovery, off by default); `LIST` (interfaces seen while discovering); `GET`/`SET a,b,c`/`CLEAR` (in-kernel filter). Enabling `--control` also auto-binds the DLT server (see `--dlt-serve`) so `DLT on` has an endpoint. |
| `--version`, `-V` | Print the build version and exit (no root / BPF needed). |

```sh
# Keep only PowerManager + ActivityManager traffic, stream to DLT Viewer, no console
adb shell /data/local/tmp/bindfetto --sink none --dlt-serve \
  --iface android.os.IPowerManager,android.app.IActivityManager

# Capture parcel payloads for one interface and decode the arguments offline
python3 ../catalog/bindfetto_catalog.py --args -o catalog.json IPowerManager.aidl
adb shell /data/local/tmp/bindfetto --iface android.os.IPowerManager --parcel on \
  | ../decode/target/release/bindfetto-decode --catalog catalog.json
# ... IPowerManager.acquireWakeLock(lock=<binder>, flags=1, tag="scr"), 96B
```

### Decoding parcel payloads (M6)

The device emits only raw parcel bytes (`--parcel`); method **arguments** are decoded
offline, keeping the hot path cheap and the logs re-decodable. Build the catalog with
`--args` (v2, carries per-method argument types), then pipe the capture through
`bindfetto-decode` — it reads the `parcel=<hex>` token and renders `method(a=1, b="x")`,
marking `…(truncated)` past the cap and `<Type>, …(unparsed)` for arguments with no fixed
layout (binders, arrays, parcelables). See `catalog/README.md` and `decode/README.md`.

See the repo-root `ROADMAP.md` for the milestone sequence.
