//! Open a URL in the user's default browser.
//!
//! Shells out to the platform opener (`open` on macOS, `xdg-open` on other
//! unices, `cmd /C start` on Windows) and returns immediately — the browser
//! launches detached so it never blocks the caller (CLI or TUI).

use std::process::{Command, Stdio};

/// Launch `url` in the default browser. Returns an error only if the opener
/// process couldn't be spawned at all.
pub fn open_url(url: &str) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = Command::new("open");
        c.arg(url);
        c
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut cmd = {
        let mut c = Command::new("xdg-open");
        c.arg(url);
        c
    };
    #[cfg(target_os = "windows")]
    let mut cmd = {
        // The empty "" is `start`'s title argument; without it a quoted URL
        // would be swallowed as the window title.
        let mut c = Command::new("cmd");
        c.args(["/C", "start", "", url]);
        c
    };

    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(())
}
