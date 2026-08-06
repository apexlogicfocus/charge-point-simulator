//! System clipboard access, isolated to this one module so the rest of the app only ever sees
//! a `Result` - never a panic.
//!
//! Backed by `arboard` rather than an OSC 52 escape sequence: this app already assumes a real
//! terminal (`crossterm`'s raw mode, `color-eyre`), `arboard` builds and works cleanly on macOS
//! (this project's primary target - verified against the real pasteboard during development),
//! and it also covers Linux/X11/Wayland and Windows without the caveats OSC 52 has (terminal
//! emulator support is inconsistent, and some emulators require an explicit opt-in for security
//! reasons). Every call site treats failure as routine - a missing display server, a locked
//! clipboard, whatever - and surfaces it as a status message rather than letting it propagate.

/// Copies `text` to the system clipboard. Never panics: both the clipboard handle and the
/// write itself can fail (no display server, clipboard held by another process, etc.), and
/// both are reported back as a plain string rather than unwound.
pub fn copy_to_clipboard(text: &str) -> Result<(), String> {
    let mut clipboard =
        arboard::Clipboard::new().map_err(|err| format!("clipboard unavailable: {err}"))?;
    clipboard
        .set_text(text.to_string())
        .map_err(|err| format!("copy failed: {err}"))
}

/// The system clipboard is process-wide, shared, mutable state - `cargo test` runs on multiple
/// threads, so anything in this crate that writes to (or reads back) the *real* clipboard must
/// serialize on this lock first, or two tests racing on it will read back each other's value.
/// `app.rs`'s `y`-key tests take this lock too, for the same reason.
#[cfg(test)]
pub(crate) static CLIPBOARD_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    // Exercises the real system clipboard rather than a fake, since the whole point of this
    // module is a thin, otherwise-untestable wrapper around `arboard`. Skipped rather than
    // failed when no clipboard is reachable (e.g. a headless CI runner with no display server),
    // since that's an environment fact this module can't control.
    #[test]
    fn copy_to_clipboard_round_trips_through_the_real_clipboard() {
        let _guard = CLIPBOARD_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        let Ok(mut clipboard) = arboard::Clipboard::new() else {
            eprintln!("skipping: no system clipboard available in this environment");
            return;
        };

        let unique = format!("charge_point_simulator_tui test {}", std::process::id());
        assert!(copy_to_clipboard(&unique).is_ok());

        assert_eq!(clipboard.get_text().unwrap(), unique);
    }
}
