// Embed the Windows executable icon (the Agentron mark, black) so agent_web.exe
// shows the brand glyph in Explorer, the taskbar, and Alt-Tab. Windows-only —
// on every other target this is a no-op, keeping the musl/guest build clean.
fn main() {
    #[cfg(windows)]
    {
        println!("cargo:rerun-if-changed=assets/agentron.ico");
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/agentron.ico");
        // A failure here (e.g. no resource compiler) shouldn't hard-fail the
        // build — the exe just ships without the icon.
        if let Err(e) = res.compile() {
            println!("cargo:warning=exe icon embed skipped: {e}");
        }
    }
}
