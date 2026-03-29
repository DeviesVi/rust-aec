fn main() {
    #[cfg(target_os = "windows")]
    let _ = embed_resource::compile("resources/app.rc", embed_resource::NONE);
}
