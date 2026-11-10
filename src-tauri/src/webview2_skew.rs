//! Detect a WebView2 runtime that was swapped under a running process.
//!
//! The runtime lives in a versioned folder, and Windows updates it whenever it
//! likes — including while RustBox is running. A browser process started before
//! the swap keeps executing the old binaries, and those binaries are the ones it
//! needs to spawn child processes. Once the update has replaced them, the next
//! request to create a webview fails with `HRESULT(0x80010108)`
//! (`RPC_E_DISCONNECTED`, "The object invoked has disconnected from its
//! clients"), because the browser process it is talking to can no longer do its
//! job.
//!
//! That is why the failure arrives "after a while" and never right after launch:
//! a freshly started process is on the version that is actually installed. Only a
//! restart fixes it, so the useful thing this module can do is name the cause
//! instead of leaving the user with a bare HRESULT.
//!
//! Detection is deliberately simple and safe: compare the runtime folders on disk
//! against when this process started. No COM calls, no shared state that can fail
//! on its own, and `None` whenever the answer is not certain.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

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
  out.sort_by(|a, b| b.version.cmp(&a.version));
  out
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
  pub fn explanation(&self) -> String {
    format!(
      "Windows updated the WebView2 runtime to {} while RustBox was running on {}. \
       A running app cannot move to the new runtime, so new tabs will fail until \
       RustBox restarts.",
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
/// found, or nothing newer than this process. Callers then keep their own
/// behaviour, so this never invents a cause it cannot see.
pub fn current_skew() -> Option<Skew> {
  if let Some(fake) = forced_skew() {
    return Some(fake);
  }
  let started = *STARTED.get()?;
  let root = runtime_root()?;
  skew_from_folders(&runtime_folders(&root), started)
}

/// A swap reported by `RB_FAKE_WEBVIEW2_SKEW`, for testing the prompt path.
///
/// A real swap cannot be staged: it needs Windows to update the runtime while
/// this process runs, which no test can cause. Without this the "ask before
/// building" path could only be exercised by unit tests on the pure decision,
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

/// The swap as a sentence, for the error surface.
pub fn runtime_swap_explanation() -> Option<String> {
  current_skew().map(|s| s.explanation())
}

#[cfg(test)]
mod tests {
  use super::*;

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
    assert!(text.contains("restarts"), "{text}");
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
}
