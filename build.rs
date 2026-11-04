//! Bakes release identity into the binary.
//!
//! The CI pipeline versions releases from git tags (`cw-vX.Y.Z`) and never
//! edits Cargo.toml, so a release build receives its version through the
//! `CI_VERSION_CW` environment variable instead. Without it (a local or
//! from-source build) the Cargo.toml version is used, and the binary knows it
//! is not a release build — `selfupdate.rs` then stays silent, since there is
//! no released artifact it could correspond to.

fn main() {
    println!("cargo:rerun-if-env-changed=CI_VERSION_CW");
    let ci_version = std::env::var("CI_VERSION_CW")
        .ok()
        .filter(|v| !v.trim().is_empty());
    let version = ci_version
        .clone()
        .unwrap_or_else(|| std::env::var("CARGO_PKG_VERSION").expect("set by cargo"));
    println!("cargo:rustc-env=CW_VERSION={}", version.trim());
    println!(
        "cargo:rustc-env=CW_RELEASE_BUILD={}",
        if ci_version.is_some() { "1" } else { "" }
    );
    println!(
        "cargo:rustc-env=CW_TARGET={}",
        std::env::var("TARGET").expect("set by cargo")
    );
}
