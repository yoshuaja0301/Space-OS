fn main() {
    let ld = std::env::var("DEP_SPACEUSER_LD").expect("libspace exports its linker script");
    println!("cargo:rustc-link-arg-bins=-T{ld}");
    println!("cargo:rustc-link-arg-bins=-zmax-page-size=0x1000");
}
