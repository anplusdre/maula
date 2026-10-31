// Embeds assets/maula.ico as the exe's icon (resource id 1; the window loads it from there too). No-op off Windows.
fn main() {
    println!("cargo:rerun-if-changed=assets/maula.ico");
    embed_resource::compile("assets/maula.rc", embed_resource::NONE).manifest_optional().unwrap();
}
