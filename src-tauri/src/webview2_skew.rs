//! Keep RustBox on one WebView2 runtime, and notice when Windows installs another.
//!
//! The runtime lives in a versioned folder, and Windows updates it whenever it
//! likes — including while RustBox is running. A browser process started before
//! the swap keeps executing the old binaries, and those binaries are the ones it
//! needs to spawn child processes.
//!
//! The failure this file exists for is not the old version disappearing. The
//! loader picks the *newest* runtime folder on every
//! `CreateCoreWebView2EnvironmentWithOptions` call when it is given no explicit
//! folder, so the next webview asks for a version the live browser process is
//! not running, and the call fails with `HRESULT(0x80010108)`
//! (`RPC_E_DISCONNECTED`). The installed version is fine; the request is simply
//! aimed at the wrong one.
//!
//! So there are two jobs here:
//!
//! 1. **Pin** — [`pin_runtime_to_startup_version`] records the folder this
//!    process started on and exports it as `WEBVIEW2_BROWSER_EXECUTABLE_FOLDER`,
//!    the documented way to name a runtime folder explicitly (the "fixed
//!    version" mode of WebView2). Every webview created afterwards resolves the
//!    same folder, so a mid-session update cannot break tab creation. Tabs keep
//!    working, and the new runtime is picked up at the next restart.
//! 2. **Notice** — [`current_skew`] compares the folders on disk against when
//!    this process started, so the UI can say "an update is waiting for a
//!    restart" instead of the user never learning about it.
//!
//! Pinning is skipped when an administrator has set the
//! `BrowserExecutableFolder` policy: the policy is a deliberate deployment
//! decision, and the loader would apply it anyway, so overriding it would
//! silently ignore IT.
//!
//! Detection is deliberately simple and safe: compare the runtime folders on disk
//! against when this process started. No COM calls, no shared state that can fail
//! on its own, and `None` whenever the answer is not certain.

use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// The loader's own switch for naming a runtime folder explicitly.
///
/// Documented on `CreateCoreWebView2EnvironmentWithOptions`: when
/// `browserExecutableFolder` is `null`, the loader consults this variable before
/// falling back to the newest installed runtime. Setting it once at start is
/// therefore enough to freeze the choice for the whole process.
const ENV_FOLDER: &str = "WEBVIEW2_BROWSER_EXECUTABLE_FOLDER";

/// Test hook: pin to a specific version instead of the newest on disk.
///
/// A real swap cannot be staged inside one process — it needs Windows to update
/// the runtime while the app runs. This lets the "pinned to an older version"
/// path be exercised deliberately, the same way `RB_FAKE_WEBVIEW2_SKEW` exercises
/// the notice. Inert unless set.
const ENV_PIN: &str = "RB_PIN_WEBVIEW2";

/// Folder timestamps land slightly after the swap itself, and a process started
/// moments after an ordinary install must not be told to restart for nothing.
const GRACE: Duration = Duration::from_secs(60);

/// Where the WebView2 runtime keeps its versioned folders.
pub fn runtime_root() -> Option<PathBuf> {
  let root = Path::new(r"C:\Program Files (x86)\Microsoft\EdgeWebView\Application");
  root.is_dir().then(|| root.to_path_buf())
}

/// One runtime version and when its folder was last written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeFolder {
  pub version: String,
  pub modified: Option<SystemTime>,
}

/// Version folders under `root`, newest name first.
///
/// `SetupMetrics` is the updater's own bookkeeping, not a runtime. Counting it
/// would make every launch after an update look skewed.
pub fn runtime_folders(root: &Path) -> Vec<RuntimeFolder> {
  let Ok(entries) = std::fs::read_dir(root) else {
    return Vec::new();
  };
  let mut out: Vec<RuntimeFolder> = entries
    .filter_map(|e| e.ok())
    .filter(|e| e.path().is_dir())
    .filter_map(|e| {
      let name = e.file_name().to_string_lossy().to_string();
      // A runtime folder is a dotted version; anything else is not one.
      if !name.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        return None;
      }
      Some(RuntimeFolder {
        version: name,
        modified: e.metadata().ok().and_then(|m| m.modified().ok()),
      })
    })
    .collect();
  out.sort_by(|a, b| compare_versions(&b.version, &a.version));
  out
}

/// Compare two dotted numeric versions.
///
/// Numeric per component, not lexicographic: the runtime reaches versions whose
/// parts change digit count (`99.0` → `100.0`), and a string comparison puts
/// `99` above `100` — which would pin the older runtime and reintroduce the very
/// mismatch this module exists to prevent.
///
/// Missing components count as 0, so `157.0` and `157.0.0` are equal. A part
/// that is not a number falls back to that part's text, which keeps an
/// unexpected folder name from panicking or comparing as equal to everything.
fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
  use std::cmp::Ordering;
  let mut left = a.split('.');
  let mut right = b.split('.');
  loop {
    match (left.next(), right.next()) {
      (None, None) => return Ordering::Equal,
      (l, r) => {
        let l = l.unwrap_or("0");
        let r = r.unwrap_or("0");
        let ord = match (l.parse::<u64>(), r.parse::<u64>()) {
          (Ok(x), Ok(y)) => x.cmp(&y),
          _ => l.cmp(r),
        };
        if ord != Ordering::Equal {
          return ord;
        }
      }
    }
  }
}

/// The runtime folder this process chose, once it has been pinned.
///
/// Held so the UI can name the version actually in use and the fallback path can
/// tell "we were pinned" from "we never were".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pin {
  /// Version directory name, e.g. `157.0.4341.42`.
  pub version: String,
  /// Absolute path to that directory — what the loader is pointed at.
  pub folder: PathBuf,
}

static PIN: std::sync::Mutex<Option<Pin>> = std::sync::Mutex::new(None);

/// What this process is pinned to, if anything. `None` after a fallback cleared
/// it, which is the signal that a retry should let the loader choose again.
pub fn pinned() -> Option<Pin> {
  PIN.lock().ok().and_then(|p| p.clone())
}

/// Pin every webview this process creates to one runtime folder.
///
/// Call once, from `main`, before the first webview exists. Returns the pin, or
/// `None` when there is nothing to pin to (no runtime directory at all) or when
/// an administrator's policy already decides the folder.
///
/// This is the fix for the reported failure: with no explicit folder the loader
/// resolves the newest runtime on every call, so a tab created after an update
/// asks for a version the live browser process is not running and fails with
/// `RPC_E_DISCONNECTED`. Naming the folder this process started on keeps every
/// later webview on the runtime that is actually loaded. Tabs survive the update;
/// the new version is picked up at the next restart, which the UI offers rather
/// than demands.
pub fn pin_runtime_to_startup_version() -> Option<Pin> {
  let root = runtime_root()?;
  // An admin-set policy is the deployment's decision, and the loader applies it
  // regardless. Pinning over it would ignore IT and leave the two disagreeing
  // about which runtime the app is on.
  if policy_browser_folder() {
    return None;
  }
  // Someone above us in the process tree may already have chosen a runtime.
  // That choice is in force for every child, so adopting it is the only honest
  // answer: overwriting it would change behaviour the caller asked for.
  if let Some(existing) = inherited_folder() {
    let version = existing
      .file_name()
      .map(|n| n.to_string_lossy().to_string())
      .unwrap_or_else(|| existing.display().to_string());
    let pin = Pin {
      version,
      folder: existing,
    };
    if let Ok(mut guard) = PIN.lock() {
      *guard = Some(pin.clone());
    }
    return Some(pin);
  }
  let requested = std::env::var(ENV_PIN).ok();
  let version = version_to_pin(&runtime_folders(&root), requested.as_deref())?;
  let folder = root.join(&version);
  // A version recorded in the directory listing can still be deleted between
  // the read and the use, so the choice is only made when the folder is real.
  if !folder.join("msedgewebview2.exe").is_file() {
    return None;
  }
  set_folder_env(&folder);
  let pin = Pin { version, folder };
  if let Ok(mut guard) = PIN.lock() {
    *guard = Some(pin.clone());
  }
  Some(pin)
}

/// Which version to pin, given the folders on disk and an optional override.
///
/// Newest on disk by default, because that is the version this process actually
/// loaded — the loader picked the same one a moment ago. The override exists for
/// tests and for deliberately staying on a known-good build.
///
/// An override naming a version that is not installed is ignored rather than
/// obeyed: the loader would fail every webview with a folder that does not
/// exist, which is strictly worse than not pinning at all.
///
/// The newest is chosen here rather than taken from the front of the slice, so
/// the answer does not depend on how the caller happened to order the list.
fn version_to_pin(folders: &[RuntimeFolder], requested: Option<&str>) -> Option<String> {
  let requested = requested.map(str::trim).filter(|v| !v.is_empty());
  if let Some(want) = requested
    && folders.iter().any(|f| f.version == want)
  {
    return Some(want.to_string());
  }
  folders
    .iter()
    .map(|f| f.version.as_str())
    .max_by(|a, b| compare_versions(a, b))
    .map(str::to_string)
}

/// The folder another process already chose for us, if any.
fn inherited_folder() -> Option<PathBuf> {
  let raw = current_folder_env()?;
  let path = PathBuf::from(raw);
  path.is_dir().then_some(path)
}

/// Serialises this module's own access to the process environment.
///
/// `std::env::set_var` is unsafe because another thread could be reading the
/// environment at the same time. The WebView2 loader reads the variable through
/// the OS (which copies it under its own lock), so the only reader this code has
/// to coordinate with is itself — hence one lock around every read and write.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Point the loader at `folder` for the rest of this process.
#[cfg(windows)]
fn set_folder_env(folder: &Path) {
  let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
  // SAFETY: the write is serialised against this module's own reads by
  // `ENV_LOCK`, and the one caller runs from `main` before any webview exists.
  unsafe { std::env::set_var(ENV_FOLDER, folder.as_os_str()) };
}

/// Nothing to pin on a platform that has no WebView2.
#[cfg(not(windows))]
fn set_folder_env(_folder: &Path) {}

#[cfg(windows)]
fn remove_folder_env() {
  let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
  // SAFETY: same contract as `set_folder_env`. This is only reached from the
  // pin fallback, which runs on the main thread after a build failed, and the
  // lock keeps it from racing this module's own `env::var` call.
  unsafe { std::env::remove_var(ENV_FOLDER) };
}

#[cfg(not(windows))]
fn remove_folder_env() {}

#[cfg(windows)]
fn current_folder_env() -> Option<String> {
  let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
  std::env::var(ENV_FOLDER).ok().filter(|v| !v.trim().is_empty())
}

#[cfg(not(windows))]
fn current_folder_env() -> Option<String> {
  None
}

/// Whether an administrator's policy already names the runtime folder.
///
/// Checked before pinning because the loader consults policy too: overriding it
/// would make the app ignore a deliberate deployment decision, and leave the
/// policy's owner believing the app runs the runtime they selected.
#[cfg(windows)]
fn policy_browser_folder() -> bool {
  use windows_sys::Win32::Foundation::{ERROR_MORE_DATA, ERROR_SUCCESS};
  use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, RegCloseKey, RegEnumValueW,
    RegOpenKeyExW,
  };

  const SUBKEY: &str = r"Software\Policies\Microsoft\Edge\WebView2\BrowserExecutableFolder";

  /// The key is keyed by app id, so the presence of *any* non-empty value means
  /// this machine has an opinion about where the runtime lives.
  fn any_value_set(root: HKEY) -> bool {
    let subkey: Vec<u16> = SUBKEY.encode_utf16().chain(std::iter::once(0)).collect();
    let mut key: HKEY = std::ptr::null_mut();
    // SAFETY: `subkey` is NUL-terminated and outlives the call; `key` is a valid
    // out-pointer. The handle is closed below on the success path.
    let opened = unsafe { RegOpenKeyExW(root, subkey.as_ptr(), 0, KEY_READ, &mut key) };
    if opened != ERROR_SUCCESS {
      return false;
    }
    let mut index = 0u32;
    let mut found = false;
    loop {
      let mut name = [0u16; 256];
      let mut name_len = name.len() as u32;
      // A `REG_SZ` value is UTF-16, so the buffer is typed as `u16` and handed to
      // the API as bytes; that avoids re-interpreting it afterwards.
      let mut data = [0u16; 512];
      let mut data_len = (data.len() * size_of::<u16>()) as u32;
      // SAFETY: every buffer is sized by its own length variable, as the API
      // requires; the pointers are to stack storage that outlives the call.
      let rc = unsafe {
        RegEnumValueW(
          key,
          index,
          name.as_mut_ptr(),
          &mut name_len,
          std::ptr::null(),
          std::ptr::null_mut(),
          data.as_mut_ptr().cast::<u8>(),
          &mut data_len,
        )
      };
      if rc == ERROR_MORE_DATA {
        // The value is longer than the buffer, so it is definitely not empty:
        // treat it as an opinion rather than silently reporting "no policy".
        // Guessing wrong here would override an administrator's choice.
        found = true;
        break;
      }
      if rc != ERROR_SUCCESS {
        break;
      }
      // A policy value that is present but empty is the documented way of saying
      // "no override for this app", so it does not count as an opinion.
      let units = data_len as usize / size_of::<u16>();
      let text = String::from_utf16_lossy(
        &data[..units.min(data.len())]
          .iter()
          .copied()
          .take_while(|c| *c != 0)
          .collect::<Vec<u16>>(),
      );
      if !text.trim().is_empty() {
        found = true;
        break;
      }
      index += 1;
    }
    // SAFETY: `key` came from a successful `RegOpenKeyExW`.
    unsafe { RegCloseKey(key) };
    found
  }

  any_value_set(HKEY_LOCAL_MACHINE) || any_value_set(HKEY_CURRENT_USER)
}

#[cfg(not(windows))]
fn policy_browser_folder() -> bool {
  false
}

/// Stop pinning, so the next webview lets the loader choose again.
///
/// Used when the pinned folder disappears mid-session: continuing to point at a
/// deleted runtime would fail every tab, and the honest recovery is to try the
/// newest one and tell the user a restart is due.
pub fn clear_runtime_pin() {
  if let Ok(mut guard) = PIN.lock() {
    *guard = None;
  }
  remove_folder_env();
}

/// Whether the folder this process is pinned to is gone.
///
/// The one failure the pin can cause on its own: Windows deleted the runtime
/// version after launch. Callers use this to tell "the pin is stale" from "the
/// build failed for its own reasons", so only the former is worth a retry.
pub fn pinned_folder_missing() -> bool {
  pinned().is_some_and(|p| !p.folder.is_dir())
}

/// A runtime swap that happened after this process started.
///
/// Carries the two versions because the confirmation prompt names them: "updated
/// to 157 while this was running on 156" is a fact the user can check, whereas
/// "there may be a problem" is not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skew {
  /// The version this process started on.
  pub from: String,
  /// The version now installed on disk.
  pub to: String,
}

impl Skew {
  /// One sentence the user can act on.
  ///
  /// Says the app keeps working, because it does: the pin holds every new tab on
  /// the running version. The restart is the only way to *use* the new runtime,
  /// so it is offered as the next step rather than as a fix for a breakage.
  pub fn explanation(&self) -> String {
    format!(
      "Windows updated the WebView2 runtime to {} while RustBox was running on {}. \
       RustBox stays on {} so tabs keep opening; restart it to move to {}.",
      self.to, self.from, self.from, self.to
    )
  }

  /// The same fact, worded for a build that did fail.
  ///
  /// Reached only when pinning was skipped (an admin policy, or a runtime that
  /// could not be resolved), so this is the one case where a restart really is
  /// the way out and the message has to say so plainly.
  pub fn failure_hint(&self) -> String {
    format!(
      "Windows updated the WebView2 runtime to {} while RustBox was running on {}. \
       Restart RustBox to pick up the new runtime.",
      self.to, self.from
    )
  }
}

/// Decide from a folder listing whether the runtime changed after `started`.
///
/// Pure on purpose: the tests feed it synthetic timestamps instead of racing the
/// filesystem's clock.
pub fn skew_from_folders(folders: &[RuntimeFolder], started: SystemTime) -> Option<Skew> {
  let cutoff = started + GRACE;
  let newer: Vec<&RuntimeFolder> = folders
    .iter()
    .filter(|f| f.modified.is_some_and(|m| m > cutoff))
    .collect();
  if newer.is_empty() {
    return None;
  }
  // Newest name first, so the first entry is what the updater installed.
  let to = newer.first()?.version.clone();
  // The version this process is most likely still on: the newest one that is
  // not part of the swap.
  let from = folders
    .iter()
    .find(|f| !newer.iter().any(|n| n.version == f.version))
    .map(|f| f.version.clone())
    .unwrap_or_else(|| "an earlier version".to_string());
  Some(Skew { from, to })
}

/// Record when this process started, so later checks compare against launch.
///
/// Called once from `main`. Without it, "started" would be first use — hours
/// later for a long-running app, which is exactly the case being detected.
pub fn mark_started() {
  STARTED.set(SystemTime::now()).ok();
}

static STARTED: std::sync::OnceLock<SystemTime> = std::sync::OnceLock::new();

/// The swap visible for this process, or `None`.
///
/// `None` covers every uncertain case: no start time recorded, no runtime folder
/// found, or nothing newer than this process. Callers then stay quiet, so this
/// never invents news the user cannot check.
///
/// This no longer decides whether a tab may open — the pin already keeps tabs
/// working — so it is purely what the notice is built from.
pub fn current_skew() -> Option<Skew> {
  if let Some(fake) = forced_skew() {
    return Some(fake);
  }
  let started = *STARTED.get()?;
  let root = runtime_root()?;
  skew_from_folders(&runtime_folders(&root), started)
}

/// A swap reported by `RB_FAKE_WEBVIEW2_SKEW`, for testing the notice path.
///
/// A real swap cannot be staged: it needs Windows to update the runtime while
/// this process runs, which no test can cause. Without this the "an update is
/// waiting" path could only be exercised by unit tests on the pure decision,
/// leaving the event, the strip, and the buttons unproven.
///
/// Inert unless the variable is set, exactly like `RB_PROBE_OPEN`. The value is
/// the version to claim was installed; anything empty or unset means "not faked".
fn forced_skew() -> Option<Skew> {
  let to = std::env::var("RB_FAKE_WEBVIEW2_SKEW").ok()?;
  let to = to.trim().to_string();
  if to.is_empty() {
    return None;
  }
  // The "before" version has to be a *different* one, or the prompt reads as
  // "updated from X to X" and looks like a bug in the message rather than a
  // test hook.
  let from = runtime_root()
    .map(|root| runtime_folders(&root))
    .and_then(|folders| {
      folders
        .into_iter()
        .map(|f| f.version)
        .find(|v| v != &to)
    })
    .unwrap_or_else(|| "an earlier version".to_string());
  Some(Skew { from, to })
}

/// The swap as a sentence, for a build that failed anyway.
///
/// `None` while a pin is still in force: the runtime is then exactly the version
/// this process started on, so the swap cannot be what failed the build and
/// claiming otherwise would send the user to restart for no reason.
pub fn runtime_swap_explanation() -> Option<String> {
  if pinned().is_some() {
    return None;
  }
  current_skew().map(|s| s.failure_hint())
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Tests that write `WEBVIEW2_BROWSER_EXECUTABLE_FOLDER` are process-wide and
  /// `cargo test` runs them in parallel, so they take turns rather than
  /// overwriting each other's pin mid-assertion.
  static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

  fn at(offset_secs: i64) -> SystemTime {
    let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    if offset_secs >= 0 {
      base + Duration::from_secs(offset_secs as u64)
    } else {
      base - Duration::from_secs((-offset_secs) as u64)
    }
  }

  fn start() -> SystemTime {
    at(0)
  }

  fn folder(version: &str, offset_secs: i64) -> RuntimeFolder {
    RuntimeFolder {
      version: version.to_string(),
      modified: Some(at(offset_secs)),
    }
  }

  /// The reported case: the app has been up for hours and the updater swapped
  /// the runtime underneath it. This must be named, not left as a raw HRESULT.
  #[test]
  fn a_runtime_written_after_start_is_reported_as_a_skew() {
    let folders = [folder("156.0.4314.58", -40_000), folder("157.0.4341.42", 45_000)];
    let skew = skew_from_folders(&folders, start()).expect("skew should be detected");
    assert_eq!(skew.from, "156.0.4314.58");
    assert_eq!(skew.to, "157.0.4341.42");
    let text = skew.explanation();
    assert!(text.contains("157.0.4341.42"), "{text}");
    assert!(text.contains("156.0.4314.58"), "{text}");
    // The notice must not read as a breakage: tabs keep working on the pinned
    // runtime, and saying otherwise is what made the old prompt look urgent.
    assert!(text.contains("tabs keep opening"), "{text}");
    assert!(text.contains("restart"), "{text}");
  }

  /// The ordinary case: nothing was installed since launch, so any failure has
  /// another cause and this module must stay out of the way.
  #[test]
  fn no_skew_when_every_runtime_predates_the_process() {
    let folders = [folder("156.0.4314.58", -40_000), folder("157.0.4341.42", -60)];
    assert!(skew_from_folders(&folders, start()).is_none());
  }

  /// A process that starts moments after an install must not be told to restart.
  #[test]
  fn a_runtime_installed_just_before_start_is_not_a_skew() {
    let folders = [folder("157.0.4341.42", 5)];
    assert!(skew_from_folders(&folders, start()).is_none());
  }

  #[test]
  fn an_empty_listing_reports_nothing() {
    assert!(skew_from_folders(&[], start()).is_none());
  }

  /// A folder whose mtime cannot be read is not evidence of anything.
  #[test]
  fn an_unknown_timestamp_is_not_a_skew() {
    let folders = [RuntimeFolder {
      version: "157.0.4341.42".to_string(),
      modified: None,
    }];
    assert!(skew_from_folders(&folders, start()).is_none());
  }

  /// `SetupMetrics` is updater bookkeeping. Reading the real directory proves it
  /// is skipped, so an update cannot look like a skew just by touching it.
  #[test]
  fn the_updater_bookkeeping_folder_is_not_a_runtime_version() {
    let base = std::env::temp_dir().join(format!("rbx-wv2skew-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(base.join("SetupMetrics")).expect("setupmetrics dir");
    std::fs::create_dir_all(base.join("157.0.4341.42")).expect("version dir");

    let versions: Vec<String> = runtime_folders(&base).into_iter().map(|f| f.version).collect();
    assert_eq!(versions, vec!["157.0.4341.42".to_string()]);

    let _ = std::fs::remove_dir_all(&base);
  }

  /// The default choice is the newest folder, which is the one the loader would
  /// have picked a moment earlier — pinning it must not change behaviour.
  #[test]
  fn the_newest_runtime_is_pinned_by_default() {
    let folders = [folder("156.0.4314.58", -40_000), folder("157.0.4341.42", -60)];
    assert_eq!(
      version_to_pin(&folders, None).as_deref(),
      Some("157.0.4341.42")
    );
  }

  /// A test or a deliberate downgrade can name a specific installed version.
  #[test]
  fn an_installed_version_can_be_requested_explicitly() {
    let folders = [folder("156.0.4314.58", -40_000), folder("157.0.4341.42", -60)];
    assert_eq!(
      version_to_pin(&folders, Some(" 156.0.4314.58 ")).as_deref(),
      Some("156.0.4314.58")
    );
  }

  /// Requesting a version that is not there must not pin at all: pointing the
  /// loader at a missing folder fails every webview, which is worse than letting
  /// it choose. Falling back to the newest is the working behaviour.
  #[test]
  fn an_uninstalled_version_falls_back_to_the_newest() {
    let folders = [folder("157.0.4341.42", -60)];
    assert_eq!(
      version_to_pin(&folders, Some("999.0.0.0")).as_deref(),
      Some("157.0.4341.42")
    );
  }

  /// An empty override is not a request, exactly like an unset one.
  #[test]
  fn a_blank_override_is_ignored() {
    let folders = [folder("157.0.4341.42", -60)];
    assert_eq!(version_to_pin(&folders, Some("   ")).as_deref(), Some("157.0.4341.42"));
  }

  /// Nothing installed means nothing to pin, and the caller must be told so it
  /// can leave the environment alone.
  #[test]
  fn nothing_installed_means_no_pin() {
    assert_eq!(version_to_pin(&[], None), None);
    assert_eq!(version_to_pin(&[], Some("157.0.4341.42")), None);
  }

  /// The version that matters is the numerically newest, not the
  /// lexicographically last. A string sort puts `99` above `100`, which would
  /// pin the *older* runtime and bring back the mismatch this whole module
  /// exists to prevent.
  #[test]
  fn versions_compare_numerically_not_as_text() {
    use std::cmp::Ordering;
    assert_eq!(compare_versions("157.0.4341.46", "157.0.4341.42"), Ordering::Greater);
    assert_eq!(compare_versions("99.0.1.1", "100.0.1.1"), Ordering::Less);
    assert_eq!(compare_versions("157.0.4341.42", "157.0.4341.42"), Ordering::Equal);
    // A short version is not "less than" a longer one with zero parts.
    assert_eq!(compare_versions("157.0", "157.0.0"), Ordering::Equal);
    assert_eq!(compare_versions("157.1", "157.0.9"), Ordering::Greater);
  }

  /// The listing is ordered by the same comparison, and the pin takes the first
  /// entry — so an update is picked up even across a digit-count change.
  #[test]
  fn the_listing_puts_the_numerically_newest_first() {
    let base = std::env::temp_dir().join(format!("rbx-wv2sort-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(base.join("99.0.1.1")).expect("old dir");
    std::fs::create_dir_all(base.join("100.0.1.1")).expect("new dir");

    let first = runtime_folders(&base).into_iter().next().map(|f| f.version);
    assert_eq!(first.as_deref(), Some("100.0.1.1"));

    let _ = std::fs::remove_dir_all(&base);
  }

  /// The pin has to reach the loader, not just a field in this module.
  ///
  /// Every other test here checks the bookkeeping. This one asks WebView2
  /// itself, through `GetAvailableCoreWebView2BrowserVersionString`, which
  /// version it resolves once the pin is set — the same call the loader makes
  /// on the app's behalf. Without it the whole change could be a well-tested
  /// no-op.
  ///
  /// Runs only where a runtime is installed, and restores whatever pin the rest
  /// of the process had so it cannot leak into another test.
  #[cfg(windows)]
  #[test]
  fn the_pin_changes_what_the_loader_resolves() {
    use webview2_com::Microsoft::Web::WebView2::Win32::GetAvailableCoreWebView2BrowserVersionString;
    use windows_core::{PCWSTR, PWSTR};
    use windows_sys::Win32::System::Com::{
      CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED,
    };

    let Some(root) = runtime_root() else {
      return;
    };
    let folders = runtime_folders(&root);
    let Some(newest) = folders.first().map(|f| f.version.clone()) else {
      return;
    };
    // Pick a different installed version to pin to, otherwise "unchanged" would
    // also be the result of doing nothing.
    let Some(other) = folders.iter().map(|f| f.version.clone()).find(|v| v != &newest) else {
      return;
    };

    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let previous = std::env::var(ENV_FOLDER).ok();
    // The test drives the variable directly: `pin_runtime_to_startup_version`
    // also writes the module's PIN static, and restoring that would race the
    // tests that reset it.
    set_folder_env(&root.join(&other));
    let resolved = {
      let _com = ComGuard::new();
      let mut out = PWSTR::null();
      // SAFETY: `null` asks for the default resolution path, which consults the
      // variable just set; `out` is a valid out-pointer.
      let r = unsafe { GetAvailableCoreWebView2BrowserVersionString(PCWSTR::null(), &mut out) };
      r.ok().map(|()| webview2_com::take_pwstr(out))
    };
    match previous {
      Some(v) => set_folder_env(Path::new(&v)),
      None => remove_folder_env(),
    }

    let resolved = resolved.expect("the loader must report a version when one is installed");
    assert_eq!(
      resolved, other,
      "the loader ignored the pinned folder and resolved the newest runtime instead"
    );

    /// Balances `CoInitializeEx` for the loader call.
    struct ComGuard;
    impl ComGuard {
      fn new() -> Self {
        // SAFETY: paired with `CoUninitialize` in `Drop`. A failure (already
        // initialised) is ignored, matching how the runtime itself treats it.
        unsafe { CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32) };
        Self
      }
    }
    impl Drop for ComGuard {
      fn drop(&mut self) {
        // SAFETY: matches the successful-or-already-done init above.
        unsafe { CoUninitialize() };
      }
    }
  }

  /// The real entry point `main` calls: it must pick an installed version, set
  /// the variable to that folder, and remember the choice.
  ///
  /// The other tests exercise the pieces. This one proves they are wired
  /// together, because a correct helper nobody calls would leave the reported
  /// bug unfixed.
  #[cfg(windows)]
  #[test]
  fn the_startup_pin_wires_the_folder_and_remembers_it() {
    let Some(root) = runtime_root() else {
      return;
    };
    let Some(expected) = runtime_folders(&root).first().map(|f| f.version.clone()) else {
      return;
    };

    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let previous_env = std::env::var(ENV_FOLDER).ok();
    let previous_pin = PIN.lock().unwrap().take();

    // Start from a clean slate: the pin adopts an inherited variable when one is
    // present (correct in production, misleading in a test).
    remove_folder_env();
    let pin = pin_runtime_to_startup_version();

    let env_after = std::env::var(ENV_FOLDER).ok();
    let remembered = pinned();
    // Restore before asserting, so a failed assertion cannot leave a pin set for
    // every later test in this process.
    if let Ok(mut guard) = PIN.lock() {
      *guard = previous_pin;
    }
    match previous_env {
      Some(v) => set_folder_env(Path::new(&v)),
      None => remove_folder_env(),
    }

    // A machine with the policy set legitimately declines to pin, and that is
    // the documented behaviour rather than a failure.
    if policy_browser_folder() {
      assert!(pin.is_none(), "the admin policy must win over the app's own pin");
      return;
    }

    let pin = pin.expect("an installed runtime must produce a pin");
    assert_eq!(pin.version, expected, "the newest runtime is the one to pin");
    assert_eq!(
      pin.folder,
      root.join(&expected),
      "the pin must name the version folder the loader expects"
    );
    assert_eq!(
      env_after.as_deref(),
      Some(pin.folder.to_string_lossy().as_ref()),
      "the variable the loader reads must point at the pinned folder"
    );
    assert_eq!(
      remembered.map(|p| p.version),
      Some(expected),
      "the pin must be remembered so the notice can name the running version"
    );
  }
}
