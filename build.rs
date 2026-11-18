fn main() {
    println!("cargo:rerun-if-changed=installer/TranscriptionServer.ico");
    #[cfg(windows)]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        winresource::WindowsResource::new()
            .set_icon("installer/TranscriptionServer.ico")
            .set("ProductName", "TranscriptionServer")
            .set("FileDescription", "TranscriptionServer")
            .compile()
            .expect("Windowsアイコンを埋め込めません");
    }
}
