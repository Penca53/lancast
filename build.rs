// Gives lancast.exe its icon (Explorer, desktop shortcuts). No-op on other platforms.
fn main() {
    println!("cargo:rerun-if-changed=assets/lancast.ico");
    embed_resource::compile("assets/lancast.rc", embed_resource::NONE).manifest_optional().unwrap();
}
