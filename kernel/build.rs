fn main() {
    // Feed the linker our own script instead of whatever default it would pick.
    // Absolute path so `cargo build` works from any directory in the workspace.
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    println!("cargo:rustc-link-arg=-T{manifest}/linker.ld");

    println!("cargo:rerun-if-changed=linker.ld");
    println!("cargo:rerun-if-changed=src/boot.S");
}
