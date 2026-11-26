fn main() {
    // puts icon.ico on the exe (desktop shortcut, taskbar, Explorer)
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("icon.ico");
        res.compile().expect("could not embed icon.ico");
    }
}
