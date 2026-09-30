fn main() {
    let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    println!("cargo:rustc-link-arg-bins=-T{dir}/linker.ld");
    println!("cargo:rustc-link-arg-bins=-zmax-page-size=0x1000");
    println!("cargo:rerun-if-changed=linker.ld");
}
