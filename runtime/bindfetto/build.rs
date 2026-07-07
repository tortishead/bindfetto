//! Build the eBPF crate for the bpf target (via aya-build) and copy its object
//! into OUT_DIR, where the consumer embeds it with `include_bytes_aligned!`.

use aya_build::{Package, Toolchain};

fn main() {
    aya_build::build_ebpf(
        [Package {
            name: "bindfetto-ebpf",
            root_dir: "../bindfetto-ebpf",
            no_default_features: false,
            features: &[],
        }],
        // Pinned nightly is fine; Toolchain::Nightly uses `nightly`.
        Toolchain::default(),
    )
    .expect("build bindfetto-ebpf for the bpf target");
}
