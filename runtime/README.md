# bindfetto runtime (eBPF probe + userspace consumer)

The on-device half of bindfetto: an [`aya`](https://aya-rs.dev) eBPF probe that
captures Binder transactions and a Rust userspace consumer that drains them.

> **Build status.** The whole runtime compiles: the **eBPF probe** for
> `bpfel-unknown-none` and the **userspace consumer** for `aarch64-linux-android`
> (with the eBPF object embedded via `aya-build`), against aya 0.13.1 / aya-ebpf
> 0.1.1 with NDK r30. It has **not been run on a device yet** — the tracepoint field
> offsets in the probe are still placeholders (see below). `aya` is Linux-only, so
> the consumer only builds for the Android target, not the macOS host.

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
> `OFF_FLAGS`) are placeholders — set them from that `format` output.

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
| `--dlt-serve [port]` | Be a DLT TCP server (default 3490); DLT Viewer connects as a TCP ECU for live trace. |
| `--iface <name>` | **In-kernel** interface filter: keep only these descriptors, dropping the rest in the probe before the ring buffer. Repeatable and comma-separated (`--iface a.b.IFoo --iface a.c.IBar,a.c.IBaz`). Match is exact (full descriptor), so `IVehicle` does not match `IVehicleCallback`. While a filter is active, transactions with no interface token (replies already excluded, special/native transactions) also drop. |

```sh
# Keep only PowerManager + ActivityManager traffic, stream to DLT Viewer, no console
adb shell /data/local/tmp/bindfetto --sink none --dlt-serve \
  --iface android.os.IPowerManager,android.app.IActivityManager
```

See the repo-root `ROADMAP.md` for the milestone sequence.
