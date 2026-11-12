//! Remember each app's open sub-tabs and where they were, across restarts.
//!
//! Without this, quitting RustBox threw the workspace shape away: an app came
//! back as its pinned first tab on the app's start page, and any sub-tab the
//! user had opened — a second Gmail, a Google search next to WhatsApp — was
//! gone. That is the difference between a browser and a bookmark.
//!
//! # Why a separate file
//!
//! `config.json` is the workspace: apps, proxies, settings. This is run state:
//! which tabs happen to be open right now. Keeping them apart means a corrupt
//! or stale session file can only lose the tab layout, never the apps, and the
//! config stays readable by a person editing it by hand. It also keeps the hot
//! write path (every navigation) away from the file that a settings change
//! rewrites wholesale.
//!
//! # Why write on every change
//!
//! The tab list is written whenever it changes: a tab opens, a tab closes, a
//! tab navigates, or the active tab moves. There is no "save on quit" pass,
//! because the case that loses the most data is exactly the one where quitting
//! never runs — a crash, an end-task, or a forced shutdown for a Windows update.
//! A save that only happens on a clean exit is a save the user cannot rely on.
//!
//! Writing is debounced rather than done inline: a page that redirects three
//! times on load would otherwise rewrite the file three times, and `pushState`
//! on a single-page app fires on every route change. The debounce is short
//! enough that a hard kill still finds the previous state on disk, and
//! [`flush`] runs before a clean exit so the last change is never the one lost.
//!
//! # What is not stored
//!
//! Popups. They are transient windows, not tabs the user built; reopening one
//! on the next launch would put a stray window on screen that they had already
//! dismissed. Their URLs are not lost — the page that opened them still has
//! them, and clicking again recreates them.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Schema version of the session file.
///
/// Independent of the config's version: they evolve for different reasons, and
/// tying them would force a session rewrite every time the workspace schema
/// moved.
const VERSION: u32 = 1;

/// How long a change waits before it is written.
///
/// Long enough to collapse the burst of navigation events a single page load
/// produces, short enough that an unexpected exit loses at most this much.
const DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(300);

/// One remembered tab.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedTab {
    /// Position in the app's strip. The pinned first tab is always 0.
    ///
    /// Stored rather than derived from array order so a hand-edited or
    /// reordered file still says what it means, and so a gap left by a tab
    /// that could not be restored does not shift every later tab.
    #[serde(default)]
    pub position: usize,
    /// Where this tab was when RustBox last saw it.
    pub url: String,
    /// The document title, for the strip before the page loads. Optional
    /// because a tab can be remembered before its first title arrives.
    #[serde(default)]
    pub title: String,
}

/// One app's remembered tabs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedApp {
    /// The app's configured start page when this was saved.
    ///
    /// Kept so a restore can tell "the user navigated away" from "the user
    /// edited the app's URL in Settings". Only the former should bring back the
    /// remembered page; the latter is a deliberate change and must win over a
    /// stale session, or editing an app's address would appear to do nothing.
    #[serde(default)]
    pub base_url: String,
    /// Index into `tabs` of the one that was on screen.
    ///
    /// An index rather than a tab id: ids are minted per session (`t2`, `t3`)
    /// and a restored tab is a new window with a new id, so a stored id would
    /// name a tab that does not exist. Out of range falls back to the first
    /// tab, which is also what a missing field means.
    #[serde(default)]
    pub active: usize,
    #[serde(default)]
    pub tabs: Vec<SavedTab>,
}

/// The whole session file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Session {
    #[serde(default)]
    pub version: u32,
    /// Keyed by app id. `BTreeMap` so the file is stable across runs and a
    /// diff of it is readable.
    #[serde(default)]
    pub apps: BTreeMap<String, SavedApp>,
}

/// Not written yet, or written and unchanged since.
static DIRTY: Mutex<bool> = Mutex::new(false);

/// Serializes the debounce: only one writer thread should be waiting, or a
/// burst of changes would spawn a thread per event.
static WRITER: Mutex<()> = Mutex::new(());

/// Where the session lives.
pub fn path() -> PathBuf {
    crate::paths::root().join("session.json")
}

impl Session {
    /// Read the session, or an empty one when there is nothing usable to read.
    ///
    /// A missing file is normal on a first run. An unreadable one is not worth
    /// failing startup over: the tabs are a convenience, and the workspace in
    /// `config.json` is what the user actually configured. The bad file is left
    /// alone rather than deleted, so it can be inspected.
    pub fn load(path: &Path) -> Session {
        let Ok(raw) = std::fs::read_to_string(path) else {
            return Session::default();
        };
        match serde_json::from_str::<Session>(raw.trim_start_matches('\u{feff}')) {
            Ok(session) => session,
            Err(e) => {
                eprintln!(
                    "[rustbox] could not read {}: {e} — starting with no remembered tabs",
                    path.display()
                );
                Session::default()
            }
        }
    }

    /// Write the session to `path`.
    ///
    /// Temp file plus rename, for the same reason the config does it: a crash
    /// mid-write must not be able to leave a truncated file that reads back as
    /// no remembered tabs at all.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let json = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, path).map_err(|e| e.to_string())
    }

    /// The tabs remembered for one app, for tests to inspect what was stored.
    #[cfg(test)]
    pub fn tabs_for(&self, app_id: &str) -> Option<&SavedApp> {
        self.apps.get(app_id)
    }

    /// Replace one app's entry, or drop it when the app has no tabs left.
    ///
    /// Dropping matters: an app the user closed entirely must not come back on
    /// the next launch, and an empty entry would be indistinguishable from an
    /// app remembered with zero tabs.
    pub fn set_app(&mut self, app_id: &str, saved: SavedApp) {
        if saved.tabs.is_empty() {
            self.apps.remove(app_id);
        } else {
            self.apps.insert(app_id.to_string(), saved);
        }
    }

    /// Remember every tab of `app_id`, in strip order.
    ///
    /// `base_url` is the app's configured start page at the time of saving, and
    /// `tabs` is `(url, title)` per tab including the pinned one. `active` is the
    /// index of the tab that was on screen. Popups are filtered out by the
    /// caller, which is the only place that knows which windows those are.
    pub fn remember(
        &mut self,
        app_id: &str,
        base_url: &str,
        tabs: &[(String, String)],
        active: usize,
    ) {
        let tabs: Vec<SavedTab> = tabs
            .iter()
            .enumerate()
            .map(|(i, (url, title))| SavedTab {
                position: i,
                url: url.clone(),
                title: title.clone(),
            })
            .collect();
        let active = if active < tabs.len() { active } else { 0 };
        self.version = VERSION;
        self.set_app(
            app_id,
            SavedApp {
                base_url: base_url.to_string(),
                active,
                tabs,
            },
        );
    }

    /// What to rebuild for `app_id`, in strip order, and which of those should
    /// end up on screen.
    ///
    /// `current_base_url` is the app's start page *now*. If it differs from what
    /// was saved, the user edited the app's address in Settings, and that edit
    /// has to win over the remembered page: restoring the old URL would make the
    /// change look like it did nothing. The rest of the tabs are still restored,
    /// because they are pages the user opened, not the app's address.
    ///
    /// Returned as owned values so the caller does not hold a borrow across the
    /// builds it triggers.
    pub fn restore_plan(&self, app_id: &str, current_base_url: &str) -> Option<(Vec<SavedTab>, usize)> {
        let saved = self.apps.get(app_id)?;
        if saved.tabs.is_empty() {
            return None;
        }
        let mut tabs = saved.tabs.clone();
        tabs.sort_by_key(|t| t.position);
        // The stored index refers to the file's own order, so resolve it against
        // the sorted list by the tab it actually named rather than by number.
        let active_url = saved.tabs.get(saved.active).map(|t| t.url.clone());
        let mut active = active_url
            .and_then(|url| tabs.iter().position(|t| t.url == url))
            .unwrap_or(0);

        if saved.base_url != current_base_url {
            // The app's address was edited. Its first tab is the app, so it takes
            // the new address; sub-tabs keep the pages they were showing.
            let base_is_unused = tabs
                .first()
                .map(|t| t.url.is_empty() || t.url == saved.base_url)
                .unwrap_or(true);
            if base_is_unused {
                if let Some(first) = tabs.first_mut() {
                    first.url = current_base_url.to_string();
                    first.title.clear();
                }
            } else {
                // The first tab had genuinely navigated elsewhere; the new
                // address still has to be honoured, so it becomes the tab.
                tabs.insert(
                    0,
                    SavedTab {
                        position: 0,
                        url: current_base_url.to_string(),
                        title: String::new(),
                    },
                );
                for (i, tab) in tabs.iter_mut().enumerate() {
                    tab.position = i;
                }
                active += 1;
            }
        }
        // A tab remembered before its first load has no URL; it would fail to
        // parse and be dropped, leaving a gap. The app's address is the honest
        // stand-in for "wherever this was going to be".
        for tab in tabs.iter_mut() {
            if tab.url.trim().is_empty() {
                tab.url = current_base_url.to_string();
            }
        }
        Some(active.min(tabs.len().saturating_sub(1))).map(|active| (tabs, active))
    }
}

/// Remember `app_id`'s tabs and schedule a write.
///
/// Best effort throughout: a failure to remember a tab must never turn into a
/// failed tab open, so nothing here returns an error the caller has to handle.
pub fn remember(app_id: &str, base_url: &str, tabs: &[(String, String)], active: usize) {
    let mut session = load_cached();
    session.remember(app_id, base_url, tabs, active);
    store_cached(session);
    mark_dirty();
}

/// What to rebuild for `app_id` on this launch, if anything.
///
/// `current_base_url` is the app's start page as configured now; see
/// [`Session::restore_plan`] for why it matters.
pub fn restore_plan(app_id: &str, current_base_url: &str) -> Option<(Vec<SavedTab>, usize)> {
    load_cached().restore_plan(app_id, current_base_url)
}

/// Drop an app from the session entirely.
pub fn forget(app_id: &str) {
    let mut session = load_cached();
    if session.apps.remove(app_id).is_some() {
        store_cached(session);
        mark_dirty();
    }
}

/// The session as last read or written by this process.
///
/// Cached because `remember` runs on every navigation: re-reading and
/// re-parsing the whole file each time would put disk I/O on the UI's response
/// path for no benefit, since this process is the only writer.
static CACHED: Mutex<Option<Session>> = Mutex::new(None);

fn load_cached() -> Session {
    let mut guard = match CACHED.lock() {
        Ok(g) => g,
        // A poisoned cache is not worth panicking over in a function whose whole
        // job is best-effort bookkeeping; start from disk instead.
        Err(poisoned) => poisoned.into_inner(),
    };
    guard
        .get_or_insert_with(|| Session::load(&path()))
        .clone()
}

fn store_cached(session: Session) {
    let mut guard = match CACHED.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    *guard = Some(session);
}

fn mark_dirty() {
    *DIRTY.lock().unwrap_or_else(|e| e.into_inner()) = true;
    // Only one writer waits at a time. A second change while it waits is already
    // covered: the writer reads the cache when it wakes, so it writes the latest
    // state rather than the state at the moment it was scheduled.
    let Ok(_writer) = WRITER.try_lock() else {
        return;
    };
    std::thread::spawn(|| {
        std::thread::sleep(DEBOUNCE);
        let _guard = WRITER.lock().unwrap_or_else(|e| e.into_inner());
        write_if_dirty();
    });
}

fn write_if_dirty() {
    let dirty = {
        let mut guard = DIRTY.lock().unwrap_or_else(|e| e.into_inner());
        let was = *guard;
        *guard = false;
        was
    };
    if !dirty {
        return;
    }
    let session = load_cached();
    if let Err(e) = session.save(&path()) {
        eprintln!("[rustbox] could not save remembered tabs: {e}");
    }
}

/// Write now, if anything is pending. Called on the way out.
///
/// A clean exit is the one moment where waiting for the debounce would lose the
/// last change for nothing, so this skips the wait. It cannot help a forced
/// kill, which is why the debounced write exists as well.
pub fn flush() {
    write_if_dirty();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rustbox-session-test-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::create_dir_all(&dir);
        dir.join("session.json")
    }

    /// The whole point of the feature: tabs survive a restart.
    #[test]
    fn tabs_survive_a_round_trip() {
        let path = temp_path("round-trip");
        let mut session = Session::default();
        session.remember(
            "app-wa",
            "https://web.whatsapp.com",
            &[
                ("https://web.whatsapp.com".to_string(), "WhatsApp".to_string()),
                ("https://google.com".to_string(), "Google".to_string()),
            ],
            1,
        );
        session.save(&path).unwrap();

        let reloaded = Session::load(&path);
        let saved = reloaded.tabs_for("app-wa").expect("app was not remembered");
        assert_eq!(saved.tabs.len(), 2, "both tabs must come back");
        assert_eq!(saved.tabs[1].url, "https://google.com");
        assert_eq!(saved.tabs[0].title, "WhatsApp");
        assert_eq!(saved.active, 1, "the tab that was on screen comes back");

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// An app the user closed must not reappear on the next launch.
    #[test]
    fn an_app_with_no_tabs_is_forgotten() {
        let mut session = Session::default();
        session.remember(
            "app-x",
            "https://a.test",
            &[("https://a.test".to_string(), String::new())],
            0,
        );
        assert!(session.tabs_for("app-x").is_some());

        session.set_app("app-x", SavedApp::default());
        assert!(
            session.tabs_for("app-x").is_none(),
            "an empty entry would come back as a phantom app"
        );
    }

    /// A corrupt or half-written file costs the tab layout and nothing else.
    #[test]
    fn a_broken_file_reads_as_no_tabs() {
        let path = temp_path("broken");
        std::fs::write(&path, "{ this is not json").unwrap();
        let session = Session::load(&path);
        assert!(session.apps.is_empty());

        std::fs::write(&path, "").unwrap();
        assert!(Session::load(&path).apps.is_empty());

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// A file from before a field existed must still load, or an upgrade would
    /// silently wipe the tabs it was meant to keep.
    #[test]
    fn a_file_without_the_optional_fields_still_loads() {
        let path = temp_path("sparse");
        std::fs::write(
            &path,
            r#"{"apps":{"app-a":{"tabs":[{"url":"https://a.test"}]}}}"#,
        )
        .unwrap();
        let session = Session::load(&path);
        let saved = session.tabs_for("app-a").expect("app was dropped");
        assert_eq!(saved.tabs[0].url, "https://a.test");
        assert_eq!(saved.tabs[0].position, 0);

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// The active index must not be able to name a tab that does not exist, or
    /// restoring it would ask for a tab that was never built.
    #[test]
    fn an_out_of_range_active_tab_falls_back_to_the_first() {
        let mut session = Session::default();
        session.remember(
            "app-y",
            "https://a.test",
            &[("https://a.test".to_string(), String::new())],
            5,
        );
        let (tabs, active) = session.restore_plan("app-y", "https://a.test").unwrap();
        assert_eq!(tabs.len(), 1);
        assert_eq!(active, 0, "an impossible index must not survive");
    }

    /// Positions are recorded in strip order, so a restore puts the tabs back
    /// where the user had them rather than in arbitrary map order.
    #[test]
    fn positions_follow_the_strip_order() {
        let mut session = Session::default();
        session.remember(
            "app-z",
            "https://one.test",
            &[
                ("https://one.test".to_string(), String::new()),
                ("https://two.test".to_string(), String::new()),
                ("https://three.test".to_string(), String::new()),
            ],
            0,
        );
        let tabs = &session.tabs_for("app-z").unwrap().tabs;
        assert_eq!(
            tabs.iter().map(|t| t.position).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        assert_eq!(tabs[2].url, "https://three.test");
    }

    /// The restore plan orders tabs by the position they were saved with, so a
    /// file whose entries are out of order still rebuilds the strip correctly.
    #[test]
    fn the_restore_plan_orders_tabs_by_position() {
        let path = temp_path("unordered");
        std::fs::write(
            &path,
            r#"{"apps":{"app-b":{"active":0,"tabs":[
                 {"position":2,"url":"https://third.test"},
                 {"position":0,"url":"https://first.test"},
                 {"position":1,"url":"https://second.test"}]}}}"#,
        )
        .unwrap();
        let session = Session::load(&path);
        let (tabs, active) = session.restore_plan("app-b", "").unwrap();
        assert_eq!(
            tabs.iter().map(|t| t.url.as_str()).collect::<Vec<_>>(),
            vec![
                "https://first.test",
                "https://second.test",
                "https://third.test"
            ]
        );
        // `active: 0` names the first entry in the *file*, which is the tab
        // stored at position 2 — so it must come back as the third tab, not the
        // first. Resolving by name rather than by number is the point.
        assert_eq!(active, 2, "the active tab follows the tab, not the index");

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// What the caller actually asks for: a remembered active tab must come back
    /// as the tab on screen, not as whatever ends up first.
    #[test]
    fn the_restore_plan_keeps_the_active_tab() {
        let mut session = Session::default();
        session.remember(
            "app-c",
            "https://one.test",
            &[
                ("https://one.test".to_string(), String::new()),
                ("https://two.test".to_string(), String::new()),
            ],
            1,
        );
        let (tabs, active) = session.restore_plan("app-c", "https://one.test").unwrap();
        assert_eq!(tabs[active].url, "https://two.test");
    }

    /// An app that was never remembered has nothing to restore, and the caller
    /// must be able to tell that apart from "remembered with one tab".
    #[test]
    fn an_unknown_app_has_no_restore_plan() {
        assert!(Session::default().restore_plan("app-nope", "").is_none());
    }

    /// Editing an app's URL in Settings has to win over the remembered page.
    /// Otherwise the user changes the address, restarts, and lands straight back
    /// on the old one — which reads as "the edit did not save".
    #[test]
    fn an_edited_start_page_replaces_the_remembered_one() {
        let mut session = Session::default();
        session.remember(
            "app-d",
            "https://old.test",
            &[("https://old.test".to_string(), "Old".to_string())],
            0,
        );
        let (tabs, active) =
            session.restore_plan("app-d", "https://new.test").unwrap();
        assert_eq!(tabs[0].url, "https://new.test");
        assert_eq!(active, 0);
        // The stale title would label the tab with the old site.
        assert!(tabs[0].title.is_empty());
    }

    /// A first tab that had navigated somewhere keeps that page: the new start
    /// page is added in front of it rather than overwriting where the user was.
    #[test]
    fn an_edited_start_page_does_not_erase_a_navigated_first_tab() {
        let mut session = Session::default();
        session.remember(
            "app-e",
            "https://old.test",
            &[
                ("https://somewhere.test".to_string(), "Elsewhere".to_string()),
                ("https://second.test".to_string(), String::new()),
            ],
            0,
        );
        let (tabs, active) = session.restore_plan("app-e", "https://new.test").unwrap();
        assert_eq!(tabs.len(), 3, "the new address is added, not substituted");
        assert_eq!(tabs[0].url, "https://new.test");
        assert_eq!(tabs[1].url, "https://somewhere.test");
        assert_eq!(active, 1, "the tab the user was on stays active");
    }

    /// A tab remembered before its first load has no URL. Restoring it must not
    /// drop a window or leave an unparseable address behind.
    #[test]
    fn a_tab_with_no_url_falls_back_to_the_start_page() {
        let path = temp_path("no-url");
        std::fs::write(
            &path,
            r#"{"apps":{"app-f":{"base_url":"https://base.test","tabs":[
                 {"position":0,"url":""},
                 {"position":1,"url":"https://second.test"}]}}}"#,
        )
        .unwrap();
        let session = Session::load(&path);
        let (tabs, _) = session.restore_plan("app-f", "https://base.test").unwrap();
        assert_eq!(tabs[0].url, "https://base.test");
        assert_eq!(tabs[1].url, "https://second.test");

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
