use std::env;

fn main() {
    let version = env::var("XKEEN_SUB_VERSION")
        .unwrap_or_else(|_| env::var("CARGO_PKG_VERSION").expect("Cargo package version is missing"));

    println!("cargo:rustc-env=XKEEN_SUB_VERSION={version}");
    println!("cargo:rerun-if-env-changed=XKEEN_SUB_VERSION");
}
