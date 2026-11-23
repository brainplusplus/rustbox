//! Moving the data root to a folder the user picks.
//!
//! The move cannot happen when the user asks for it, and that is a filesystem
//! fact rather than a shortcut: WebView2, CEF and every profile hold open
//! handles on `Cookies`, `Local State` and the engine's `.dll` for as long as the
//! window exists. Copying an open SQLite database gives a torn copy, and
//! renaming the tree out from under a running browser is worse. So the request
//! is recorded and applied at the start of the next launch, before anything has
//! opened a single profile.
//!
//! Order of operations at startup, chosen so a crash at any point leaves one
//! complete data folder:
//!
//! 1. copy the tree to the new folder and check every file arrived at its full
//!    length;
//! 2. save the config in the new folder with the request cleared;
//! 3. write the locator (the one file that makes the new folder authoritative);
//! 4. delete the old folder, best effort.
//!
//! A failure in step 1 changes nothing: the locator still points at the old
//! folder, and the reason is written to `data-move-error.txt` so the settings
//! page can show it instead of the move silently not happening.

use crate::config::Config;
use std::path::{Path, PathBuf};

/// Written next to the locator when a move failed. The settings page reads it.
pub const ERROR_FILE: &str = "data-move-error.txt";

/// What moved, for the log line and the settings page.
pub struct MoveReport {
    pub files: u64,
    pub bytes: u64,
}

impl MoveReport {
    pub fn describe(&self, to: &Path) -> String {
        format!(
            "moved {} files ({:.1} MB) to {}",
            self.files,
            self.bytes as f64 / (1024.0 * 1024.0),
            to.display()
        )
    }
}

/// Apply a pending move, if there is one. Called once, first thing in `main`.
///
/// Returns a note worth logging. Never leaves the app pointing at a half-copied
/// folder: on any error the old root stays authoritative.
pub fn startup() -> Option<String> {
    let (current, why) = crate::paths::resolve_root();
    if let Some(note) = &why {
        eprintln!("[rustbox] {note}");
    }
    // A redirected data root is a test seam for the hand-off and engine install.
    // Without a redirected locator as well, running a queued move here would
    // write the developer's real locator and hijack their app, so it is skipped.
    #[cfg(debug_assertions)]
    if crate::paths::debug_root().is_some()
        && std::env::var_os("RB_DATA_ROOT_LOCATOR").is_none()
    {
        crate::paths::init_root(current, why);
        return None;
    }

    // The root is frozen only after the move, not before: a move changes where
    // this very process has to read and write, so freezing the old path first
    // would leave the app running against a folder it just deleted.
    let (root, note) = apply(&current, &crate::paths::locator_dir());
    crate::paths::init_root(root, why);
    note
}

/// The move itself, with the locations passed in so it can be tested against
/// temp folders instead of the real profile.
///
/// Returns the root this process must use from now on: the target when the move
/// worked, the original otherwise.
fn apply(current: &Path, locator_dir: &Path) -> (PathBuf, Option<String>) {
    let config_path = current.join("config.json");
    let mut cfg = match Config::load(&config_path) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!(
                "[rustbox] not reading {} for a pending move: {e}",
                config_path.display()
            );
            return (current.to_path_buf(), None);
        }
    };
    let Some(pending) = cfg.pending_data_root.take() else {
        return (current.to_path_buf(), None);
    };
    let target = PathBuf::from(pending);

    match move_root(current, &target) {
        Ok(report) => {
            cfg.pending_data_root = None;
            // Saved before the locator: if this fails the old root still owns the
            // workspace, so the move is simply retried on the next start.
            if let Err(e) = cfg.save(&target.join("config.json")) {
                let note = format!("could not save the config in {}: {e}", target.display());
                record_error(locator_dir, &note);
                return (current.to_path_buf(), Some(note));
            }
            let locator = if key(&target) == key(&crate::paths::default_root()) {
                // Moving back to the default location needs no pointer: leaving
                // one would be a file that says "the default is the default".
                crate::paths::clear_locator_in(locator_dir)
            } else {
                crate::paths::write_locator_in(locator_dir, &target)
            };
            if let Err(e) = locator {
                record_error(locator_dir, &e);
                return (current.to_path_buf(), Some(e));
            }
            // Only now, with the new folder authoritative, may the old copy go.
            // Deleting earlier would turn a failed locator write into lost data.
            let mut note = report.describe(&target);
            if std::fs::remove_dir_all(current).is_err() {
                note.push_str(&format!(
                    "; the old folder {} could not be fully deleted and can be removed by hand",
                    current.display()
                ));
            }
            clear_error(locator_dir);
            (target, Some(note))
        }
        Err(e) => {
            // Drop the request so the same failure is not retried on every
            // launch, and keep the reason where the user will look.
            cfg.pending_data_root = None;
            let _ = cfg.save(&config_path);
            record_error(locator_dir, &e);
            (
                current.to_path_buf(),
                Some(format!("could not move the data folder: {e}")),
            )
        }
    }
}

/// Record a move for the next launch, after checking it can work.
///
/// `None` means "back to the default folder", which is why this takes an option
/// rather than a string: an empty path would have to be special-cased in the UI
/// and again here.
pub fn schedule(target: Option<&str>, config_path: &Path) -> Result<String, String> {
    let from = crate::paths::root();
    let to = match target.map(|t| t.trim().trim_matches('"')) {
        None | Some("") => crate::paths::default_root(),
        Some(text) => PathBuf::from(text),
    };
    if key(&to) == key(&from) {
        return Err(format!("RustBox already keeps its data in {}.", to.display()));
    }
    validate_target(&from, &to)?;
    if !free_for_use(&to)? {
        return Err(format!(
            "{} already contains files. Pick an empty folder (or a new one) so two workspaces cannot be merged by accident.",
            to.display()
        ));
    }
    probe_writable(&to)?;

    let mut cfg = Config::load(config_path)?;
    cfg.pending_data_root = Some(to.display().to_string());
    cfg.save(config_path)?;
    Ok(format!(
        "RustBox will move its data to {} the next time it starts, then delete {}.\n\nRestart to finish the move.",
        to.display(),
        from.display()
    ))
}

/// Forget a recorded move that has not run yet.
pub fn cancel(config_path: &Path) -> Result<String, String> {
    let mut cfg = Config::load(config_path)?;
    if cfg.pending_data_root.take().is_none() {
        return Ok("There was no move to cancel.".to_string());
    }
    cfg.save(config_path)?;
    Ok("The move was cancelled. Nothing was copied.".to_string())
}

/// The last failure, if a previous start could not move the folder.
pub fn error_note() -> Option<String> {
    error_note_in(&crate::paths::locator_dir())
}

fn error_note_in(dir: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(dir.join(ERROR_FILE)).ok()?;
    let text = raw.trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

fn record_error(dir: &Path, message: &str) {
    let _ = std::fs::create_dir_all(dir);
    let _ = std::fs::write(dir.join(ERROR_FILE), format!("{message}\n"));
}

fn clear_error(dir: &Path) {
    let _ = std::fs::remove_file(dir.join(ERROR_FILE));
}

/// Refuse targets that cannot work, before anything is copied.
///
/// Only the shape is checked here (absolute, disjoint, a folder); whether the
/// folder can take the data is [`probe_writable`]'s job.
pub fn validate_target(from: &Path, to: &Path) -> Result<(), String> {
    if to.as_os_str().is_empty() {
        return Err("Choose a folder first.".to_string());
    }
    if !to.is_absolute() {
        return Err(format!(
            "{} is not a full path. Use one like D:\\RustBox.",
            to.display()
        ));
    }
    let f = key(from);
    let t = key(to);
    if f == t {
        return Err("RustBox already keeps its data there.".to_string());
    }
    if t.starts_with(&format!("{f}\\")) {
        return Err(format!(
            "{} is inside the current data folder. Pick a folder outside it.",
            to.display()
        ));
    }
    if f.starts_with(&format!("{t}\\")) {
        return Err(format!(
            "{} contains the current data folder. Pick a folder that does not.",
            to.display()
        ));
    }
    if to.exists() && !to.is_dir() {
        return Err(format!("{} is a file, not a folder.", to.display()));
    }
    Ok(())
}

/// Whether the folder can receive the data without mixing two workspaces.
///
/// An empty folder is fine, and so is one holding only the locator: that is the
/// default location on the way back from a moved root.
fn free_for_use(dir: &Path) -> Result<bool, String> {
    if !dir.exists() {
        return Ok(true);
    }
    let entries =
        std::fs::read_dir(dir).map_err(|e| format!("could not list {}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("could not list {}: {e}", dir.display()))?;
        if entry.file_name() == crate::paths::LOCATOR {
            continue;
        }
        return Ok(false);
    }
    Ok(true)
}

/// Fail before scheduling rather than at startup, where the user cannot answer.
pub fn probe_writable(dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    if !dir.is_dir() {
        return Err(format!("{} is not a folder.", dir.display()));
    }
    let probe = dir.join(".rustbox-write-test");
    std::fs::write(&probe, b"ok").map_err(|e| format!("{} is not writable: {e}", dir.display()))?;
    let _ = std::fs::remove_file(&probe);
    Ok(())
}

/// Copy the tree into `to` and check it arrived whole. The source is left alone:
/// the caller may only delete it once the new location is authoritative, and
/// that happens after the locator is written.
pub fn move_root(from: &Path, to: &Path) -> Result<MoveReport, String> {
    validate_target(from, to)?;
    if !free_for_use(to)? {
        return Err(format!(
            "{} already contains files, so the move would merge two data folders",
            to.display()
        ));
    }
    if !from.is_dir() {
        return Err(format!("{} is not there any more", from.display()));
    }
    std::fs::create_dir_all(to).map_err(|e| format!("could not create {}: {e}", to.display()))?;

    let wanted = manifest(from)?;
    copy_tree(from, to, true)?;
    verify(to, &wanted)?;
    Ok(wanted)
}

/// Every file under `dir` and its total length, so a copy can be checked rather
/// than trusted. Bookkeeping files are excluded, since [`copy_tree`] leaves them
/// behind and the counts would never match.
fn manifest(dir: &Path) -> Result<MoveReport, String> {
    let mut report = MoveReport {
        files: 0,
        bytes: 0,
    };
    for entry in
        std::fs::read_dir(dir).map_err(|e| format!("could not read {}: {e}", dir.display()))?
    {
        let entry = entry.map_err(|e| format!("could not read {}: {e}", dir.display()))?;
        let name = entry.file_name();
        if is_bookkeeping(&name) {
            continue;
        }
        let path = entry.path();
        let kind = entry
            .file_type()
            .map_err(|e| format!("could not inspect {}: {e}", path.display()))?;
        if kind.is_dir() {
            let inner = manifest(&path)?;
            report.files += inner.files;
            report.bytes += inner.bytes;
        } else if kind.is_file() {
            let len = entry.metadata().map(|m| m.len()).unwrap_or(0);
            report.files += 1;
            report.bytes += len;
        }
    }
    Ok(report)
}

/// The locator and the error note describe a location rather than being data:
/// carrying a stale locator into the new root would point the next start
/// somewhere else entirely.
fn is_bookkeeping(name: &std::ffi::OsStr) -> bool {
    name == crate::paths::LOCATOR || name == ERROR_FILE
}

fn copy_tree(from: &Path, to: &Path, top: bool) -> Result<MoveReport, String> {
    let mut report = MoveReport {
        files: 0,
        bytes: 0,
    };
    for entry in
        std::fs::read_dir(from).map_err(|e| format!("could not read {}: {e}", from.display()))?
    {
        let entry = entry.map_err(|e| format!("could not read {}: {e}", from.display()))?;
        let name = entry.file_name();
        if top && is_bookkeeping(&name) {
            continue;
        }
        let src = entry.path();
        let dst = to.join(&name);
        let kind = entry
            .file_type()
            .map_err(|e| format!("could not inspect {}: {e}", src.display()))?;
        if kind.is_dir() {
            std::fs::create_dir_all(&dst)
                .map_err(|e| format!("could not create {}: {e}", dst.display()))?;
            let inner = copy_tree(&src, &dst, false)?;
            report.files += inner.files;
            report.bytes += inner.bytes;
        } else if kind.is_file() {
            let n = std::fs::copy(&src, &dst)
                .map_err(|e| format!("could not copy {}: {e}", src.display()))?;
            report.files += 1;
            report.bytes += n;
        }
        // Symlinks and junctions are skipped rather than followed: a profile
        // tree has no business leaving the data folder, and following one could
        // copy an unrelated directory or loop for ever.
    }
    Ok(report)
}

/// The copy must hold the same file count and byte total as the source.
fn verify(to: &Path, wanted: &MoveReport) -> Result<(), String> {
    let got = manifest(to)?;
    if got.files != wanted.files || got.bytes != wanted.bytes {
        return Err(format!(
            "the copy in {} is incomplete: {} files / {} bytes arrived, {} / {} were expected",
            to.display(),
            got.files,
            got.bytes,
            wanted.files,
            wanted.bytes
        ));
    }
    Ok(())
}

/// Compare paths the way the filesystem does: `\\?\` and trailing separators
/// ignored, case-insensitive on Windows.
fn key(path: &Path) -> String {
    let text = path.to_string_lossy().replace('/', "\\");
    let text = text.strip_prefix("\\\\?\\").unwrap_or(&text);
    let text = text.trim_end_matches('\\');
    if cfg!(windows) {
        text.to_lowercase()
    } else {
        text.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rb-data-root-{name}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_target_inside_or_above_the_current_root_is_refused() {
        // Copying a folder into itself either recurses or silently drops the
        // nested copy on the next move; a parent would swallow the root.
        let from = Path::new("C:\\Users\\a\\AppData\\Local\\rustbox");
        assert!(validate_target(from, &from.join("inner")).is_err());
        assert!(validate_target(from, from).is_err());
        assert!(validate_target(from, Path::new("C:\\Users\\a")).is_err());
        assert!(validate_target(from, Path::new("D:\\RustBox")).is_ok());
    }

    #[test]
    fn a_relative_target_is_refused() {
        // It would resolve against the working directory, which is not where
        // anyone expects their data to land.
        assert!(validate_target(Path::new("C:\\rustbox"), Path::new("mydata")).is_err());
    }

    #[test]
    fn a_move_copies_the_tree_and_leaves_the_source_for_the_caller() {
        let base = scratch("move");
        let from = base.join("from");
        fs::create_dir_all(from.join("profiles/app-a")).unwrap();
        fs::write(from.join("config.json"), b"{}").unwrap();
        fs::write(from.join("profiles/app-a/Cookies"), b"cookie").unwrap();
        fs::write(from.join(crate::paths::LOCATOR), b"D:\\elsewhere").unwrap();

        let to = base.join("to");
        let report = move_root(&from, &to).unwrap();

        assert_eq!(report.files, 2);
        // The source survives: deleting it before the locator points at `to`
        // would lose the data if that write failed.
        assert!(from.join("config.json").exists());
        assert_eq!(fs::read(to.join("profiles/app-a/Cookies")).unwrap(), b"cookie");
        // A stale locator in the new root would be read as an instruction.
        assert!(!to.join(crate::paths::LOCATOR).exists());
    }

    #[test]
    fn a_target_with_files_is_refused_instead_of_merged() {
        let base = scratch("nonempty");
        let from = base.join("from");
        fs::create_dir_all(&from).unwrap();
        fs::write(from.join("config.json"), b"{}").unwrap();
        let to = base.join("to");
        fs::create_dir_all(&to).unwrap();
        fs::write(to.join("someone-elses.txt"), b"keep me").unwrap();

        assert!(move_root(&from, &to).is_err());
        // A refused move may not delete anything on either side.
        assert!(from.join("config.json").exists());
        assert!(to.join("someone-elses.txt").exists());
    }

    #[test]
    fn a_folder_holding_only_the_locator_counts_as_empty() {
        // This is the default location on the way back from a moved root, so
        // refusing it would make the move one-way.
        let base = scratch("locator-only");
        let dir = base.join("root");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(crate::paths::LOCATOR), b"D:\\RustBox").unwrap();
        assert_eq!(free_for_use(&dir), Ok(true));

        fs::write(dir.join("config.json"), b"{}").unwrap();
        assert_eq!(free_for_use(&dir), Ok(false));
    }

    #[test]
    fn a_queued_move_runs_and_points_the_locator_at_the_new_folder() {
        // The whole path a real start takes: config says "move to B", the tree is
        // copied, the config in B no longer asks for a move, and the locator names
        // B so the *next* start finds it without being told again.
        let base = scratch("apply");
        let from = base.join("from");
        let to = base.join("to");
        let locator_dir = base.join("locator");
        fs::create_dir_all(from.join("profiles/app-a")).unwrap();
        fs::write(from.join("profiles/app-a/Cookies"), b"cookie").unwrap();
        let cfg = format!(
            r#"{{"version":5,"apps":[],"pending_data_root":"{}"}}"#,
            to.display().to_string().replace('\\', "\\\\")
        );
        fs::write(from.join("config.json"), cfg).unwrap();

        let (root, note) = apply(&from, &locator_dir);
        // The running process has to switch to the new root, or it keeps writing
        // to a folder it just deleted.
        assert_eq!(root, to);
        let note = note.expect("a move was queued");
        // The cookie plus the config itself: the config travels so the new root
        // keeps the app list.
        assert!(note.contains("moved 2 files"), "{note}");

        // Source gone, data present, request cleared, locator written.
        assert!(!from.exists());
        assert_eq!(fs::read(to.join("profiles/app-a/Cookies")).unwrap(), b"cookie");
        let saved: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(to.join("config.json")).unwrap()).unwrap();
        assert!(saved["pending_data_root"].is_null(), "{saved}");
        assert_eq!(
            crate::paths::read_locator_in(&locator_dir),
            Some(to.clone())
        );
        assert_eq!(error_note_in(&locator_dir), None);
    }

    #[test]
    fn a_failed_move_clears_the_request_and_leaves_a_reason() {
        // Retrying the same failure on every launch would be a loop the user
        // cannot see; the note is what turns it into an explanation.
        let base = scratch("apply-fail");
        let from = base.join("from");
        let locator_dir = base.join("locator");
        fs::create_dir_all(&from).unwrap();
        // A target that already holds someone else's files is refused.
        let to = base.join("to");
        fs::create_dir_all(&to).unwrap();
        fs::write(to.join("not-mine.txt"), b"x").unwrap();
        let cfg = format!(
            r#"{{"version":5,"apps":[],"pending_data_root":"{}"}}"#,
            to.display().to_string().replace('\\', "\\\\")
        );
        fs::write(from.join("config.json"), cfg).unwrap();

        let (root, note) = apply(&from, &locator_dir);
        // A refused move must leave the process on the folder that still has the
        // data, not on the one it failed to fill.
        assert_eq!(root, from);
        assert!(note.expect("a move was queued").contains("could not move"));
        // The old folder is intact and still authoritative: no locator written.
        assert!(from.join("config.json").exists());
        assert_eq!(crate::paths::read_locator_in(&locator_dir), None);
        assert!(error_note_in(&locator_dir).is_some_and(|n| n.contains("already contains files")));
        let saved: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(from.join("config.json")).unwrap()).unwrap();
        assert!(saved["pending_data_root"].is_null(), "{saved}");
    }

    #[test]
    fn no_queued_move_does_nothing_at_all() {
        let base = scratch("apply-none");
        let from = base.join("from");
        fs::create_dir_all(&from).unwrap();
        fs::write(from.join("config.json"), br#"{"version":5,"apps":[]}"#).unwrap();
        assert_eq!(apply(&from, &base.join("locator")), (from.clone(), None));
        assert!(from.join("config.json").exists());
    }

    #[test]
    fn paths_differing_only_in_case_are_the_same_folder() {        // Windows would treat them as one directory, so accepting both would
        // schedule a move that copies a folder onto itself.
        if !cfg!(windows) {
            return;
        }
        assert_eq!(key(Path::new("C:\\Data\\RustBox")), key(Path::new("c:\\data\\rustbox\\")));
        assert_eq!(key(Path::new("\\\\?\\C:\\Data")), key(Path::new("C:\\Data")));
    }
}
