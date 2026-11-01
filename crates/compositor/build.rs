//! openh264-sys2 builds `OpenH264`'s x86 assembly only when it can run nasm, and
//! otherwise falls back to C without a word. That encoder costs about 1.6x the CPU in
//! the benchmark scroll, so refuse to build it by accident.

use std::env;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=NASM");
    println!("cargo:rerun-if-env-changed=OPENH264_NO_ASM");
    let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    if !matches!(arch.as_str(), "x86" | "x86_64") || env::var_os("OPENH264_NO_ASM").is_some() {
        return;
    }
    let nasm = env::var_os("NASM").unwrap_or_else(|| "nasm".into());
    let found = Command::new(&nasm)
        .arg("-v")
        .output()
        .is_ok_and(|output| output.status.success());
    assert!(
        found,
        "nasm not found ({}): OpenH264 would build without its assembly and encode \
         about 1.6x slower. Install nasm, or set OPENH264_NO_ASM=1 to build the slow \
         encoder on purpose.",
        nasm.to_string_lossy()
    );
}
