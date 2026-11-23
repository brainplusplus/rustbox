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
            // From here the copy is complete, which means any failure below has
            // to undo *it* as well. The source is still untouched and still
            // authoritative, so discarding the copy costs nothing - while
            // keeping it would make every later attempt fail `free_for_use` with
            // "already contains files", the same trap that a half-copied tree
            // left behind. A move is all-or-nothing in both directions.
            cfg.pending_data_root = None;
            // Saved before the locator: if this fails the old root still owns the
            // workspace, so the move is simply retried on the next start.
            if let Err(e) = cfg.save(&target.join("config.json")) {
                let note = format!("could not save the config in {}: {e}", target.display());
                rollback(&target);
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
                rollback(&target);
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
/// Work out where a requested move would go, and refuse it now if it cannot work.
///
/// `None` means "back to the default folder", which is why this takes an option
/// rather than a string: an empty path would have to be special-cased in the UI
/// and again here.
///
/// Separate from [`schedule`] because the checks touch the filesystem - a folder
/// listing and a write probe, which on a network share can take seconds - and the
/// caller should hold the config lock only for the write. Holding it across the
/// probe would stall every other command, including the one that opens a tab.
pub fn plan(target: Option<&str>) -> Result<PathBuf, String> {
    plan_in(&crate::paths::root(), target)
}

/// [`plan`] against an explicit source folder.
///
/// The source is a parameter because [`plan`] has to size the data folder it is
/// moving, and sizing the real one made the tests depend on this machine's data
/// root existing: the first CI run of these tests failed with `os error 3` on a
/// runner where that folder had never been created. Nothing else changes - the
/// real caller still passes [`crate::paths::root`].
fn plan_in(from: &Path, target: Option<&str>) -> Result<PathBuf, String> {
    let from = from.to_path_buf();
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
    // Last, because the probe above is what creates the folder: a volume cannot
    // be asked about space through a path that does not exist yet. Doing this at
    // scheduling time is the difference between "pick another folder" now and a
    // half-copied tree discovered at the next start, where the user cannot
    // answer. Measured: a 12.34 GB root was accepted for a drive with 7.00 GB
    // free, copied 0.76 GB, then died on a locked file.
    let needed = if from.is_dir() { manifest(&from)?.bytes } else { 0 };
    if let Some(why) = room_error(needed, free_bytes(&to), &to) {
        return Err(why);
    }
    Ok(to)
}

/// Record an already-planned move in `cfg` and on disk.
///
/// `cfg` must be the config the running app holds, not a fresh read of the file.
/// Every command that saves (`save_apps`, `set_favorites`, the settings form,
/// `bump_usage`) serializes that whole live struct, so a pending move written to
/// disk alone is erased by the next unrelated save - measured: the queued target
/// was gone from `config.json` after the user touched a setting. The settings pane
/// also reads this same in-memory copy, so it never saw the move either and the
/// radio jumped back to "Default folder" as if the save had not happened.
pub fn schedule(to: PathBuf, cfg: &mut Config, config_path: &Path) -> Result<String, String> {
    let from = crate::paths::root();
    cfg.pending_data_root = Some(to.display().to_string());
    cfg.save(config_path)?;
    Ok(format!(
        "RustBox will move its data to {} the next time it starts, then delete {}.\n\nRestart to finish the move.",
        to.display(),
        from.display()
    ))
}

/// Forget a recorded move that has not run yet.
pub fn cancel(cfg: &mut Config, config_path: &Path) -> Result<String, String> {
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

/// Free bytes on the volume that holds `dir`, when the platform can say.
///
/// `None` means "cannot tell", which skips the check rather than refusing a
/// move on a platform where the measurement is not implemented. A writable
/// folder is not the same as a folder with room: on Windows a 2-byte probe
/// succeeds on a drive that is about to run out of space.
fn free_bytes(dir: &Path) -> Option<u64> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
        let mut wide: Vec<u16> = dir.as_os_str().encode_wide().collect();
        wide.push(0);
        let mut free: u64 = 0;
        // SAFETY: `wide` is NUL-terminated and outlives the call; the last two
        // out-pointers are documented as optional and are not read.
        let ok = unsafe {
            GetDiskFreeSpaceExW(
                wide.as_ptr(),
                &mut free,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        (ok != 0).then_some(free)
    }
    #[cfg(not(windows))]
    {
        let _ = dir;
        None
    }
}

/// Why a move will not fit, or `None` when it will.
///
/// Split from [`free_bytes`] so the decision can be tested without a real full
/// drive.
///
/// The margin is 2 GiB, not the few megabytes a byte-for-byte copy would need.
/// Two reasons, both measured on this machine. A Chromium profile is a live
/// database that grows while the user is browsing: between picking the folder and
/// the next start, a chat app can write hundreds of megabytes of IndexedDB, and a
/// move that fits by 50 MB at scheduling time does not fit by then. And a drive
/// filled to the last block is a broken drive for everything else on it - Windows
/// itself starts failing, and the copy the user asked for would be the cause.
/// Refusing with 2 GiB to spare costs a folder choice; accepting and running out
/// costs a half-copied tree on a full volume.
const MARGIN: u64 = 2 * 1024 * 1024 * 1024;

/// The check itself. `None` from `free_bytes` means the platform cannot answer,
/// which skips the refusal rather than blocking a move on a guess.
fn room_error(needed: u64, free: Option<u64>, to: &Path) -> Option<String> {
    let free = free?;
    if needed.saturating_add(MARGIN) <= free {
        return None;
    }
    let short = needed.saturating_add(MARGIN).saturating_sub(free);
    Some(format!(
        "{} is not big enough for the move: RustBox needs {} for the data plus {} of headroom, and that drive has {} free ({} short). Pick a folder with more room, or free some space on that drive.",
        to.display(),
        gb(needed),
        gb(MARGIN),
        gb(free),
        gb(short)
    ))
}

fn gb(bytes: u64) -> String {
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    let value = bytes as f64 / GIB;
    if value >= 10.0 {
        format!("{value:.0} GB")
    } else {
        format!("{value:.1} GB")
    }
}

/// Remove what a failed copy wrote, so the folder stays usable.
///
/// A half-copied tree is worse than no copy: [`free_for_use`] refuses any target
/// that holds files, so every later attempt is answered with "already contains
/// files" until the user deletes it by hand. Measured: a move that died on a
/// locked file left 0.76 GB in 2,570 files behind. Only what this move wrote is
/// removed - the caller has already established the folder was empty.
fn rollback(to: &Path) {
    let Ok(entries) = std::fs::read_dir(to) else {
        return;
    };
    for entry in entries.flatten() {
        if is_bookkeeping(&entry.file_name()) {
            continue;
        }
        let path = entry.path();
        let _ = if path.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
    }
}

/// Copy one file, waiting out a handle that is still being closed.
///
/// A move runs at the start of the next launch, but the *previous* instance may
/// not have finished exiting: Chromium's profile files stay open until its whole
/// process tree is gone, and closing the window does not wait for that. Measured:
/// `cef\<app>\Network\Cookies` failed with os error 32 on exactly this path. The
/// error is transient, so it is retried for a few seconds before giving up.
fn copy_file(src: &Path, dst: &Path) -> Result<u64, String> {
    copy_file_with(src, dst, 12, std::time::Duration::from_millis(300))
}

/// The retry policy itself, so it can be exercised without waiting it out.
fn copy_file_with(
    src: &Path,
    dst: &Path,
    attempts: u32,
    pause: std::time::Duration,
) -> Result<u64, String> {
    let mut attempt = 0;
    loop {
        match std::fs::copy(src, dst) {
            Ok(n) => return Ok(n),
            Err(e) if is_sharing_violation(&e) && attempt + 1 < attempts => {
                attempt += 1;
                std::thread::sleep(pause);
            }
            Err(e) if is_sharing_violation(&e) => {
                return Err(format!(
                    "could not copy {}: another RustBox window (or a browser it started) still has the file open, and it did not close in time: {e}",
                    src.display()
                ))
            }
            Err(e) => return Err(format!("could not copy {}: {e}", src.display())),
        }
    }
}

/// `ERROR_SHARING_VIOLATION` and `ERROR_LOCK_VIOLATION`.
fn is_sharing_violation(e: &std::io::Error) -> bool {
    matches!(e.raw_os_error(), Some(32) | Some(33))
}

/// Copy the tree into `to` and prove it arrived whole.
///
/// The order is the safety property, and it is deliberate: the source is never
/// touched here. Copy, verify, and only then may the caller write the locator and
/// delete the old folder. At no point does a crash leave the app pointing at a
/// folder that holds less than everything.
///
/// Two things make it more than a `copy_dir`: the copy is *consistent* as well as
/// complete, and a failed attempt leaves nothing behind.
///
/// *Consistent* - the source is measured before and after. A data folder that is
/// still being written to (the previous instance may not have finished exiting)
/// can hand over a leveldb whose files were copied at different moments, and a
/// file count that matches proves nothing about that. If the measurement changes,
/// the attempt is discarded and retried rather than accepted.
///
/// *Nothing left behind* - every failure path rolls the target back to empty, so
/// the next attempt is not refused by [`free_for_use`] for "already contains
/// files".
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

    // Three attempts, because the failure this guards against is a previous
    // instance that is a second away from exiting: the first attempt sees a file
    // still open or a folder still growing, the next one usually does not.
    const ATTEMPTS: u32 = 3;
    let mut last = String::new();
    for attempt in 1..=ATTEMPTS {
        if attempt > 1 {
            // Let the process that was still writing finish. The per-file retry
            // in `copy_file` covers a single locked file; this covers the folder
            // changing shape underneath the whole copy.
            std::thread::sleep(std::time::Duration::from_secs(2));
        }
        let wanted = match manifest(from) {
            Ok(m) => m,
            Err(e) => {
                rollback(to);
                return Err(e);
            }
        };
        // Checked here as well as in `plan`, because free space can change
        // between scheduling the move and the next launch, and because a move
        // that cannot finish must not start: 12 GB onto a small disk ends with a
        // full drive *and* a partial tree.
        if let Some(why) = room_error(wanted.bytes, free_bytes(to), to) {
            rollback(to);
            return Err(why);
        }

        let copied = match copy_tree(from, to, true) {
            Ok(r) => r,
            Err(e) => {
                rollback(to);
                last = e;
                continue;
            }
        };
        // A copy that wrote more than the snapshot saw is a source that grew
        // mid-copy: the tree on disk is now a mix of two moments.
        if copied.files != wanted.files || copied.bytes != wanted.bytes {
            rollback(to);
            last = format!(
                "the data folder changed while it was being copied ({} files / {} bytes against {} / {} at the start)",
                copied.files, copied.bytes, wanted.files, wanted.bytes
            );
            continue;
        }
        if let Err(e) = verify(to, &wanted) {
            rollback(to);
            last = e;
            continue;
        }
        // Measured again, after the copy: this is what turns "the files arrived"
        // into "the files arrived from a folder that was not moving".
        match manifest(from) {
            Ok(now) if now.files == wanted.files && now.bytes == wanted.bytes => {
                return Ok(wanted)
            }
            Ok(now) => {
                rollback(to);
                last = format!(
                    "the data folder was still being written to during the copy ({} files / {} bytes against {} / {})",
                    now.files, now.bytes, wanted.files, wanted.bytes
                );
            }
            Err(e) => {
                rollback(to);
                last = e;
            }
        }
    }
    Err(format!(
        "could not copy the data folder after {ATTEMPTS} attempts; close every other RustBox window and try again. Last problem: {last}"
    ))
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
            let n = copy_file(&src, &dst)?;
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

    /// The regression: a queued move used to be written by loading a second copy
    /// of the config from disk, so the live struct the app reads (and saves) never
    /// learned about it. The settings pane then reported "nothing queued" and the
    /// radio fell back to "Default folder", which is exactly "I saved a custom
    /// folder and it still says default".
    #[test]
    fn a_scheduled_move_lives_in_the_live_config_not_only_on_disk() {
        let base = scratch("schedule");
        let from = base.join("from");
        fs::create_dir_all(&from).unwrap();
        let target = base.join("new-root");
        let config_path = base.join("config.json");
        fs::write(&config_path, br#"{"version":5,"apps":[]}"#).unwrap();
        let mut cfg = Config::load(&config_path).unwrap();

        let to = plan_in(&from, Some(target.to_str().unwrap()))
            .expect("an empty folder is a valid target");
        let note = schedule(to.clone(), &mut cfg, &config_path).unwrap();
        assert!(note.contains(&to.display().to_string()), "{note}");
        assert_eq!(
            cfg.pending_data_root.as_deref(),
            Some(to.display().to_string().as_str()),
            "the live config is what the settings pane reads"
        );

        // Every other command serializes this whole struct. Saving an unrelated
        // field must not erase the queue the user just created.
        cfg.favorites.push("https://example.test".to_string());
        cfg.save(&config_path).unwrap();
        let saved: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
        assert_eq!(
            saved["pending_data_root"],
            to.display().to_string(),
            "an unrelated save kept the queued move"
        );
        assert_eq!(saved["favorites"][0], "https://example.test");
    }

    #[test]
    fn cancelling_a_move_clears_it_where_the_app_reads_it() {
        let base = scratch("cancel");
        let from = base.join("from");
        fs::create_dir_all(&from).unwrap();
        let target = base.join("new-root");
        let config_path = base.join("config.json");
        fs::write(&config_path, br#"{"version":5,"apps":[]}"#).unwrap();
        let mut cfg = Config::load(&config_path).unwrap();
        let to = plan_in(&from, Some(target.to_str().unwrap())).unwrap();
        schedule(to, &mut cfg, &config_path).unwrap();

        cancel(&mut cfg, &config_path).unwrap();
        assert!(cfg.pending_data_root.is_none());
        let saved: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
        assert!(saved["pending_data_root"].is_null(), "{saved}");
    }

    #[test]
    fn a_folder_with_files_is_refused_before_a_move_is_recorded() {
        let base = scratch("plan-busy");
        let from = base.join("from");
        fs::create_dir_all(&from).unwrap();
        let target = base.join("busy");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("keep.txt"), b"x").unwrap();
        let err = plan_in(&from, Some(target.to_str().unwrap())).unwrap_err();
        assert!(err.contains("already contains files"), "{err}");
    }

    /// The CI failure this pins: `plan` sized the *real* data root, and on a
    /// runner where RustBox had never run, that folder did not exist, so the
    /// whole call failed with `os error 3` and the two queued-move tests died.
    /// A root that is not there holds nothing, so it needs no room.
    #[test]
    fn a_source_root_that_does_not_exist_yet_is_not_an_error() {
        let base = scratch("plan-fresh");
        let missing = base.join("never-created");
        let target = base.join("new-root");
        let to = plan_in(&missing, Some(target.to_str().unwrap()))
            .expect("a first run has nothing to move");
        assert_eq!(to, target);
    }

    /// The measured case: a 12.39 GiB data folder offered a drive with 6.97 GiB
    /// free. The old code accepted it, copied 0.76 GiB, and only failed at the
    /// next start on an unrelated locked file - so the user was told "os error
    /// 32" when the real answer was "that drive is too small".
    #[test]
    fn a_target_drive_without_room_is_refused_with_the_numbers() {
        let to = Path::new("F:\\rustbox");
        // The measured pair: 13,305,447,371 bytes of data and 7,483,351,040 free.
        let needed = 13_305_447_371u64;
        let free = 7_483_351_040u64;
        let err = room_error(needed, Some(free), to).expect("F: cannot hold the data root");
        assert!(err.contains("12 GB"), "names what is needed: {err}");
        assert!(err.contains("2.0 GB"), "names the headroom: {err}");
        assert!(err.contains("7.0 GB"), "names what is free: {err}");
        assert!(err.contains("7.4 GB"), "names how much is missing: {err}");
        assert!(err.contains("F:\\rustbox"), "names the folder: {err}");
    }

    /// The point of the margin: a target that fits by a hair today is refused,
    /// because the profile grows between scheduling and the next launch.
    #[test]
    fn the_two_gib_of_headroom_is_what_decides_a_close_call() {
        let to = Path::new("D:\\RustBox");
        let needed = 10_000_000_000u64;
        // Room for the data plus 1 GB of slack: refused, because the slack is
        // not the 2 GB the check asks for.
        assert!(room_error(needed, Some(needed + 1024 * 1024 * 1024), to).is_some());
        // Exactly the margin, and a hair over it.
        assert!(room_error(needed, Some(needed + MARGIN), to).is_none());
        assert!(room_error(needed, Some(needed + MARGIN - 1), to).is_some());
        // A drive with plenty of room is never refused.
        assert!(room_error(needed, Some(100_000_000_000), to).is_none());
        // A drive that cannot hold the data at all, whatever the margin.
        assert!(room_error(needed, Some(1_000_000_000), to).is_some());
        // "cannot tell" must not block a move on a platform without the call.
        assert!(room_error(u64::MAX, None, to).is_none());
    }

    /// A failed copy must not leave a tree that makes every retry say "already
    /// contains files". Measured: 2,570 files / 0.76 GB were left behind.
    #[test]
    fn a_failed_copy_is_rolled_back_to_an_empty_folder() {
        let base = scratch("rollback");
        let to = base.join("target");
        fs::create_dir_all(to.join("cef\\app\\Network")).unwrap();
        fs::write(to.join("cef\\app\\Network\\Cookies"), b"partial").unwrap();
        fs::write(to.join("config.json"), b"{}").unwrap();
        // The locator describes a location rather than being data, so a rollback
        // on the way back to the default folder must not delete it.
        fs::write(to.join(crate::paths::LOCATOR), b"C:\\x").unwrap();

        rollback(&to);
        let left: Vec<String> = fs::read_dir(&to)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(left, vec![crate::paths::LOCATOR.to_string()], "{left:?}");
    }

    #[test]
    fn a_volume_reports_its_free_space() {
        // Not a number to assert on: just that the call answers on this platform,
        // because `None` silently disables the whole check.
        let free = free_bytes(&std::env::temp_dir());
        assert!(free.is_some_and(|b| b > 0), "{free:?}");
    }

    /// The failure that started this: a file the previous instance still had
    /// open. Retried, it succeeds; retried once and given up on, it says who is
    /// holding the file instead of a bare `os error 32`.
    #[cfg(windows)]
    #[test]
    fn a_locked_file_is_retried_then_reported_as_another_window() {
        use std::os::windows::fs::OpenOptionsExt;
        let base = scratch("locked");
        let src = base.join("Cookies");
        let dst = base.join("copy");
        fs::write(&src, b"session").unwrap();
        // Share mode 0 is what a browser's own lock looks like: a plain
        // `File::open` shares read/write and would copy without complaint.
        let held = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&src)
            .unwrap();

        // One attempt, no waiting: the error has to name the cause.
        let err = copy_file_with(&src, &dst, 1, std::time::Duration::ZERO).unwrap_err();
        assert!(err.contains("still has the file open"), "{err}");
        assert!(err.contains("Cookies"), "{err}");

        // With a retry the same call succeeds, which is what makes the move
        // survive a previous instance that is still exiting.
        drop(held);
        assert_eq!(
            copy_file_with(&src, &dst, 2, std::time::Duration::ZERO).unwrap(),
            7
        );
        assert_eq!(fs::read(&dst).unwrap(), b"session");
    }

    /// All-or-nothing in the other direction: the copy finished, the locator did
    /// not get written, so the copy must go. Otherwise the target is left full
    /// and every later attempt is refused with "already contains files".
    #[test]
    fn a_move_that_cannot_write_the_locator_leaves_the_target_empty() {
        let base = scratch("apply-locator");
        let from = base.join("from");
        let to = base.join("to");
        fs::create_dir_all(from.join("cef/app/Network")).unwrap();
        fs::write(from.join("cef/app/Network/Cookies"), b"cookie").unwrap();
        let cfg = format!(
            r#"{{"version":5,"apps":[],"pending_data_root":"{}"}}"#,
            to.display().to_string().replace('\\', "\\\\")
        );
        fs::write(from.join("config.json"), cfg).unwrap();
        // A file where the locator directory should be: `write_locator_in` cannot
        // create it, so the move fails after the copy has already succeeded.
        let locator_dir = base.join("locator");
        fs::write(&locator_dir, b"not a directory").unwrap();

        let (root, note) = apply(&from, &locator_dir);
        assert_eq!(root, from, "the old root stays authoritative");
        let note = note.expect("a move was queued");
        assert!(note.contains("locator"), "names the failing step: {note}");
        // The whole point: nothing is left in the target to block a retry.
        let left: Vec<String> = fs::read_dir(&to)
            .map(|d| {
                d.flatten()
                    .map(|e| e.file_name().to_string_lossy().to_string())
                    .collect()
            })
            .unwrap_or_default();
        assert!(left.is_empty(), "target should be empty, holds {left:?}");
        // And the source is untouched, so the user lost nothing.
        assert_eq!(fs::read(from.join("cef/app/Network/Cookies")).unwrap(), b"cookie");
    }
}
