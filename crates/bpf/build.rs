use std::env;
use std::path::PathBuf;

use libbpf_cargo::SkeletonBuilder;

const COLLECTOR_SRC: &str = "src/bpf/collector.bpf.c";
const CGROUP_TEST_SRC: &str = "src/bpf/cgroup_inode_test.bpf.c";

fn main() {
    let manifest_dir = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR must be set in build script"),
    );

    let collector_out = manifest_dir
        .join("src")
        .join("bpf")
        .join("collector.skel.rs");

    let cgroup_test_out = manifest_dir
        .join("src")
        .join("bpf")
        .join("cgroup_inode_test.skel.rs");

    let arch = env::var("CARGO_CFG_TARGET_ARCH")
        .expect("CARGO_CFG_TARGET_ARCH must be set in build script");
    println!("cargo:warning=bpf arch={}", arch);

    let vmlinux_path = vmlinux::include_path_root().join(&arch);
    let sync_timer_include = manifest_dir
        .join("..")
        .join("bpf-sync-timer")
        .join("include");
    let vmlinux_str = vmlinux_path
        .to_str()
        .expect("vmlinux include path must be valid UTF-8");
    let sync_timer_str = sync_timer_include
        .to_str()
        .expect("sync timer include path must be valid UTF-8");

    // Build the collector skeleton
    SkeletonBuilder::new()
        .source(COLLECTOR_SRC)
        .clang_args(["-I", vmlinux_str, "-I", sync_timer_str])
        .build_and_generate(&collector_out)
        .unwrap();

    // Build the cgroup test skeleton
    SkeletonBuilder::new()
        .source(CGROUP_TEST_SRC)
        .clang_args(["-I", vmlinux_str, "-I", sync_timer_str])
        .build_and_generate(&cgroup_test_out)
        .unwrap();

    // Set rerun-if-changed for all relevant files
    println!("cargo:rerun-if-changed={COLLECTOR_SRC}");
    println!("cargo:rerun-if-changed={CGROUP_TEST_SRC}");
    println!("cargo:rerun-if-changed=src/bpf/collector.h");
    println!("cargo:rerun-if-changed=src/tests.rs");
}
