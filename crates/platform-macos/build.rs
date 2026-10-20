#[path = "../../build/macos-swift-rpath.rs"]
mod swift_rpath;

fn main() {
    swift_rpath::link_swift_concurrency();
}
