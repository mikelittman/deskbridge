// ScreenCaptureKit's Swift objects record `@rpath/libswift_Concurrency.dylib`.
// Cargo does not forward a dependency's `rustc-link-arg` to the final binary,
// so the rpath has to be added by the package that is actually linked.
// macOS keeps this library in the dyld shared cache at `/usr/lib/swift` even
// when the file is not present on disk.

pub fn link_swift_concurrency() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../../build/macos-swift-rpath.rs");
    println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
    if let Some(developer) = developer_dir() {
        for relative in [
            "Toolchains/XcodeDefault.xctoolchain/usr/lib/swift/macosx",
            "Toolchains/XcodeDefault.xctoolchain/usr/lib/swift-5.5/macosx",
            "usr/lib/swift/macosx",
            "usr/lib/swift-5.5/macosx",
        ] {
            let dir = format!("{developer}/{relative}");
            if std::path::Path::new(&dir).join("libswift_Concurrency.dylib").is_file() {
                println!("cargo:rustc-link-arg=-Wl,-rpath,{dir}");
            }
        }
    }
}

fn developer_dir() -> Option<String> {
    if let Ok(dir) = std::env::var("DEVELOPER_DIR") {
        if !dir.is_empty() {
            return Some(dir);
        }
    }
    let output = std::process::Command::new("xcode-select").arg("-p").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let dir = String::from_utf8(output.stdout).ok()?;
    let dir = dir.trim();
    if dir.is_empty() { None } else { Some(dir.to_string()) }
}
