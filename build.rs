fn main() {
    // `nexusweave upgrade` picks the release asset built for this exact target
    println!("cargo:rustc-env=NEXUSWEAVE_TARGET={}", std::env::var("TARGET").unwrap());
    println!("cargo:rerun-if-changed=build.rs");
}
