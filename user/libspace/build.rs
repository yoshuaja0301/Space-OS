//! Exposes the user linker script path to dependent binaries via `DEP_SPACEUSER_LD`.
fn main() {
    let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    println!("cargo:ld={dir}/user.ld");
    println!("cargo:rerun-if-changed=user.ld");
}
