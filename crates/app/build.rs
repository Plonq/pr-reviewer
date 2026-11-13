use std::{env, path::PathBuf, process::Command};

fn run(command: &mut Command) {
    assert!(command.status().expect("run native compiler").success());
}

fn main() {
    println!("cargo:rerun-if-changed=native/updater.m");
    println!("cargo:rerun-if-env-changed=SPARKLE_DIR");
    if env::var_os("CARGO_FEATURE_AUTO_UPDATES").is_none() {
        return;
    }
    assert_eq!(env::var("CARGO_CFG_TARGET_OS").unwrap(), "macos");
    let root = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("../..");
    let sparkle = env::var_os("SPARKLE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("target/sparkle"));
    assert!(
        sparkle.join("Sparkle.framework").exists(),
        "run bash scripts/fetch-sparkle.sh first"
    );
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let sdk = Command::new("xcrun")
        .args(["--show-sdk-path"])
        .output()
        .unwrap();
    assert!(sdk.status.success());
    run(Command::new("clang")
        .args([
            "-fobjc-arc",
            "-fblocks",
            "-Werror",
            "-Wall",
            "-Wno-unused-parameter",
            "-arch",
        ])
        .arg(if env::var("CARGO_CFG_TARGET_ARCH").unwrap() == "aarch64" {
            "arm64"
        } else {
            "x86_64"
        })
        .args(["-mmacosx-version-min=15.0", "-isysroot"])
        .arg(String::from_utf8(sdk.stdout).unwrap().trim())
        .arg(format!("-F{}", sparkle.display()))
        .args(["-c", "native/updater.m", "-o"])
        .arg(out.join("updater.o")));
    run(Command::new("ar")
        .arg("crs")
        .arg(out.join("libgander_updater.a"))
        .arg(out.join("updater.o")));
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-search=framework={}", sparkle.display());
    println!("cargo:rustc-link-lib=static=gander_updater");
    println!("cargo:rustc-link-lib=framework=Sparkle");
    println!("cargo:rustc-link-lib=framework=AppKit");
    println!("cargo:rustc-link-arg=-Wl,-rpath,@executable_path/../Frameworks");
}
