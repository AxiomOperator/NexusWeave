fn main() {
    // `xgraph upgrade` picks the release asset built for this exact target
    println!("cargo:rustc-env=XGRAPH_TARGET={}", std::env::var("TARGET").unwrap());
    println!("cargo:rerun-if-changed=build.rs");
}
