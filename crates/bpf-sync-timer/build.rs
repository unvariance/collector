use std::env;
use std::path::PathBuf;

use libbpf_cargo::SkeletonBuilder;

const SYNC_TIMER_SRC: &str = "src/bpf/sync_timer.bpf.c";

fn main() {
    let manifest_dir =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR must be set"));

    let out = manifest_dir
        .join("src")
        .join("bpf")
        .join("sync_timer.skel.rs");

    let arch = env::var("CARGO_CFG_TARGET_ARCH").expect("CARGO_CFG_TARGET_ARCH must be set");
    println!("cargo:warning=bpf-sync-timer arch={}", arch);
    let vmlinux_path = vmlinux::include_path_root().join(&arch);
    let include_dir = manifest_dir.join("include");
    let vmlinux_str = vmlinux_path
        .to_str()
        .expect("vmlinux include path must be valid UTF-8");
    let include_str = include_dir
        .to_str()
        .expect("include path must be valid UTF-8");

    SkeletonBuilder::new()
        .source(SYNC_TIMER_SRC)
        .clang_args(["-I", vmlinux_str, "-I", include_str])
        .build_and_generate(&out)
        .expect("failed to generate sync timer skeleton");

    println!("cargo:rerun-if-changed={SYNC_TIMER_SRC}");
    println!("cargo:rerun-if-changed=include/sync_timer.bpf.h");
    println!("cargo:rerun-if-changed=include/sync_timer_bitmap.bpf.h");
}
