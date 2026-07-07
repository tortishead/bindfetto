# bindfetto runtime (eBPF probe + userspace consumer)

The on-device half of bindfetto: an [`aya`](https://aya-rs.dev) eBPF probe that
captures Binder transactions and a Rust userspace consumer that drains them.

> **Build status.** The **eBPF probe** (`bindfetto-ebpf`) and **`bindfetto-common`**
> compile cleanly for `bpfel-unknown-none` against aya-ebpf 0.1.1 / aya 0.13.1 —
> verified. The **userspace consumer** (`bindfetto`) has *not* been built here: `aya`
> is Linux-only (won't compile on the macOS host) and cross-building to
> `aarch64-linux-android` needs the Android NDK linker. Its `build.rs` /
> ring-buffer API calls are intended shape — reconcile on first Android build. If
> the aya-build glue fights you, regenerate boilerplate with
> `cargo generate aya-template` and drop these `src/` files + `bindfetto-common` in.

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

Expected M1 output, one line per transaction:

```
1234 -> 5678: code=7 flags=0x0 size=0 oneway
```

(pids only — process names arrive in M2, real interface/method in M3.)

See the repo-root `ROADMAP.md` for the milestone sequence.
