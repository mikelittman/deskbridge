fn main() {
    #[cfg(target_os = "macos")]
    swift_rpath::link_swift_concurrency();
}

#[cfg(target_os = "macos")]
#[path = "../../build/macos-swift-rpath.rs"]
mod swift_rpath;
