//! Installs a macOS app update into the running app's bundle.
//!
//! The updater plugin exchanges the whole `.app` folder with the new one.
//! The Dock keeps an app it holds by the folder's file id, not its path,
//! so after such an update its entry points at the old folder, which is
//! then deleted, and the restarted app shows up as a second app beside it.
//! This exchanges only the bundle's `Contents` instead: the `.app` folder,
//! and with it the Dock's entry, stays the same.

use std::fs::{File, FileTimes};
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use flate2::read::GzDecoder;

/// The `.app` folder the running program belongs to, when it runs from one
/// (`<name>.app/Contents/MacOS/<program>`).
pub fn running_bundle() -> Option<PathBuf> {
    let program = std::env::current_exe().ok()?;
    let bundle = program.parent()?.parent()?.parent()?;
    let is_bundle = bundle.extension().is_some_and(|ext| ext == "app")
        && program.parent()?.ends_with("Contents/MacOS");
    is_bundle.then(|| bundle.to_path_buf())
}

/// Puts the `Contents` of the app in `archive` (the updater's `.app.tar.gz`,
/// already checked against its signature) into `bundle`, in one atomic
/// exchange. On an error before the exchange, `bundle` is unchanged.
pub fn install(bundle: &Path, archive: &[u8]) -> io::Result<()> {
    // The exchange only works within one file system, so the new app is
    // unpacked next to the bundle.
    let folder = bundle
        .parent()
        .ok_or_else(|| io::Error::other("the app has no parent folder"))?;
    let staged = tempfile::Builder::new()
        .prefix(".leiser-harness-update")
        .tempdir_in(folder)?;
    // `unpack` refuses entries that would land outside `staged`.
    tar::Archive::new(GzDecoder::new(archive)).unpack(staged.path())?;
    let contents = unpacked_app(staged.path())?.join("Contents");
    exchange(&contents, &bundle.join("Contents"))?;
    // The old `Contents` is now in `staged`, which is deleted on drop.
    // A new modification time tells macOS to read the bundle again; the
    // update is in place without it.
    let _ = File::open(bundle)
        .and_then(|bundle| bundle.set_times(FileTimes::new().set_modified(SystemTime::now())));
    Ok(())
}

/// The one `.app` folder the archive unpacked into `staged`.
fn unpacked_app(staged: &Path) -> io::Result<PathBuf> {
    let mut apps = std::fs::read_dir(staged)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "app"));
    match (apps.next(), apps.next()) {
        (Some(app), None) => Ok(app),
        _ => Err(io::Error::other(
            "the update does not hold exactly one .app",
        )),
    }
}

/// Exchanges what `a` and `b` name, atomically (APFS supports it; other file
/// systems return an error and change nothing).
fn exchange(a: &Path, b: &Path) -> io::Result<()> {
    use rustix::fs::{CWD, RenameFlags, renameat_with};
    renameat_with(CWD, a, CWD, b, RenameFlags::EXCHANGE)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::MetadataExt;

    use flate2::Compression;
    use flate2::write::GzEncoder;

    use super::*;

    /// An updater archive holding `Demo.app` with one file of `text`.
    fn archive(text: &str) -> Vec<u8> {
        let mut builder = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::fast()));
        let mut header = tar::Header::new_gnu();
        header.set_size(text.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder
            .append_data(&mut header, "Demo.app/Contents/MacOS/demo", text.as_bytes())
            .unwrap();
        builder.into_inner().unwrap().finish().unwrap()
    }

    #[test]
    fn replaces_the_contents_and_keeps_the_bundle_folder() {
        let folder = tempfile::tempdir().unwrap();
        let bundle = folder.path().join("Demo.app");
        std::fs::create_dir_all(bundle.join("Contents/MacOS")).unwrap();
        std::fs::write(bundle.join("Contents/MacOS/demo"), "old").unwrap();
        let folder_id = std::fs::metadata(&bundle).unwrap().ino();

        install(&bundle, &archive("new")).unwrap();

        let program = std::fs::read_to_string(bundle.join("Contents/MacOS/demo")).unwrap();
        assert_eq!(program, "new");
        assert_eq!(std::fs::metadata(&bundle).unwrap().ino(), folder_id);
        // Only the bundle is left: the staged update and old contents are gone.
        let left: Vec<_> = std::fs::read_dir(folder.path()).unwrap().collect();
        assert_eq!(left.len(), 1);
    }

    #[test]
    fn leaves_the_bundle_as_it_was_when_the_archive_is_broken() {
        let folder = tempfile::tempdir().unwrap();
        let bundle = folder.path().join("Demo.app");
        std::fs::create_dir_all(bundle.join("Contents/MacOS")).unwrap();
        std::fs::write(bundle.join("Contents/MacOS/demo"), "old").unwrap();

        assert!(install(&bundle, b"not an archive").is_err());

        let program = std::fs::read_to_string(bundle.join("Contents/MacOS/demo")).unwrap();
        assert_eq!(program, "old");
    }
}
