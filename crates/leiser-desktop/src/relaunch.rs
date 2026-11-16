//! Starts the app again after it updated itself on macOS.
//!
//! Tauri's own restart runs `open -n` on the bundle while the old app still
//! runs. `-n` asks for a second instance, and the Dock shows every instance
//! of an app as a tile of its own: after an update the kept Dock entry sat
//! idle and the new app ran in a second tile beside it. Here a small shell
//! waits until the old app has exited and only then opens the bundle,
//! without `-n`, so Launch Services starts it as the one instance the Dock
//! entry stands for.

use std::io;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};

/// How long the shell waits for the old app to exit before it opens the
/// bundle anyway, in tenths of a second.
const WAIT_TENTHS: u32 = 300;

/// Waits for `pid` to exit, then runs `open` with `bundle`.
const SCRIPT: &str = r#"i=0
while kill -0 "$1" 2>/dev/null && [ "$i" -lt "$2" ]; do sleep 0.1; i=$((i + 1)); done
exec "$3" "$4""#;

/// Starts a process, detached from this one, that opens `bundle` once this
/// process has exited. The caller exits next.
pub fn after_exit(bundle: &Path) -> io::Result<()> {
    waiting_open(std::process::id(), bundle, Path::new("/usr/bin/open"))
        .spawn()
        .map(drop)
}

/// The command that waits for `pid` to exit, then runs `open bundle`.
fn waiting_open(pid: u32, bundle: &Path, open: &Path) -> Command {
    let mut command = Command::new("/bin/sh");
    command
        .args(["-c", SCRIPT, "sh"])
        .arg(pid.to_string())
        .arg(WAIT_TENTHS.to_string())
        .arg(open)
        .arg(bundle)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        // Its own process group, so nothing sent to the app's group on
        // exit reaches it.
        .process_group(0);
    command
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    /// A stand-in for `open` that writes its argument to `marker`.
    fn fake_open(folder: &Path, marker: &Path) -> std::path::PathBuf {
        let open = folder.join("open");
        std::fs::write(
            &open,
            format!("#!/bin/sh\nprintf '%s' \"$1\" > '{}'\n", marker.display()),
        )
        .unwrap();
        std::fs::set_permissions(&open, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        open
    }

    fn wait_for(path: &Path) -> Option<String> {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Ok(text) = std::fs::read_to_string(path) {
                return Some(text);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        None
    }

    #[test]
    fn opens_the_bundle_only_after_the_old_app_has_exited() {
        let folder = tempfile::tempdir().unwrap();
        let marker = folder.path().join("opened");
        let open = fake_open(folder.path(), &marker);
        let mut old_app = Command::new("/bin/sleep").arg("60").spawn().unwrap();
        let bundle = Path::new("/Applications/Demo.app");

        let mut waiting = waiting_open(old_app.id(), bundle, &open).spawn().unwrap();

        std::thread::sleep(Duration::from_millis(500));
        assert!(!marker.exists(), "opened while the old app still ran");
        old_app.kill().unwrap();
        old_app.wait().unwrap();
        assert_eq!(wait_for(&marker).as_deref(), Some("/Applications/Demo.app"));
        assert!(waiting.wait().unwrap().success());
    }

    #[test]
    fn opens_the_bundle_at_once_when_the_old_app_is_gone() {
        let folder = tempfile::tempdir().unwrap();
        let marker = folder.path().join("opened");
        let open = fake_open(folder.path(), &marker);
        let mut gone = Command::new("/usr/bin/true").spawn().unwrap();
        gone.wait().unwrap();

        let started = Instant::now();
        let mut waiting = waiting_open(gone.id(), Path::new("/Applications/Demo.app"), &open)
            .spawn()
            .unwrap();

        assert!(waiting.wait().unwrap().success());
        assert_eq!(
            std::fs::read_to_string(&marker).unwrap(),
            "/Applications/Demo.app"
        );
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
