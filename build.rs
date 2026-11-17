fn main() {
    // Tetap dibutuhkan agar icon muncul di file explorer/task manager
    if std::path::Path::new("icon.ico").exists() {
        let mut res = winres::WindowsResource::new();
        res.set_icon("icon.ico");
        res.compile().unwrap();
    }
}