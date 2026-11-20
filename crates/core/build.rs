fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let target = std::env::var("TARGET").expect("Cargo provides the build target");
    println!("cargo:rustc-env=INKCRE_BUILD_TARGET={target}");
}
