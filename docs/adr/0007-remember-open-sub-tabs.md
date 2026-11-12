# ADR 0007: Remember each app's open sub-tabs and their last URL

**Status:** Accepted (2026-11-12)

## Context

RustBox keeps an app's windows only as long as the process lives. `OPEN_TABS` in
`tabs.rs` is an in-memory map of app id to `AppTabs`, and nothing wrote it to
disk. So quitting RustBox discarded the workspace *shape*: every app came back as
its pinned first tab, on the app's start page.

Concretely, the reported behaviour was:

1. Open the WhatsApp app.
2. Open a sub-tab in it, e.g. `google.com`.
3. Quit RustBox.
4. Start it again. The Google tab is gone, and WhatsApp is back on its start page.

That is the difference between a browser and a set of bookmarks, and it is the
one thing that made the sub-tab feature not worth using: a tab you cannot rely on
to still be there is a tab you have to re-open every day.

## Decision

Persist each app's open tabs, in strip order, with the URL and document title of
each, plus which tab was on screen. Restore them when the app is next opened.

### Where it is stored

A separate `session.json` next to `config.json`, not a field inside it.

- `config.json` is the workspace: apps, proxies, settings. Losing it is a real
  loss. `session.json` is run state. A corrupt or stale session file must be able
  to cost only the tab layout, and keeping them apart is what guarantees that.
- The write path is hot (every navigation). Rewriting the whole workspace config
  on every page load would put the user's app definitions through a rename dance
  several times a minute for no benefit.
- A user editing `config.json` by hand stays able to do so without a large
  machine-generated tab blob in the way.

### When it is written

On every change: a tab is built, a tab is closed, a tab navigates, a tab's title
changes, the active tab moves, a tab is reordered. Not only on quit.

The save-on-quit design was rejected because the case that loses the most data is
exactly the one where quitting never runs — a crash, an End Task, or a forced
shutdown for a Windows update. A save that only happens on a clean exit is a save
the user cannot rely on. Verified by opening two sub-tabs and killing the process
with `Stop-Process -Force`: `session.json` on disk already had all three tabs.

Writes are debounced by 300 ms and coalesced through a single writer, so a page
that redirects three times on load, or a single-page app that calls `pushState`
on every route change, does not rewrite the file each time. `session::flush()`
runs on `RunEvent::Exit` to close the debounce window on a clean quit; it is the
optimisation, not the mechanism.

### What is restored, and what wins on conflict

- **Order** comes from the stored `position`, not from array order, so a
  hand-edited or reordered file still rebuilds the strip correctly.
- **The active tab** is resolved by the URL it named, not by index alone. If the
  tab cannot be found, the first one is used rather than an impossible index.
- **An edited start page wins.** The session records the app's configured
  `base_url` alongside its tabs. If it differs from the app's URL now, the user
  edited the address in Settings, and restoring the remembered page would make
  that edit look like it did nothing. The new address replaces the first tab when
  that tab was still the start page; if the first tab had genuinely navigated
  elsewhere, the new address is inserted in front so neither page is lost.
- **Popups are not remembered.** They are transient windows rather than tabs the
  user built, and reopening one would put a window on screen they had already
  dismissed.
- **The rest are built hidden.** Restoring nine tabs must not flash nine windows
  over the tab the user is reading; the remembered active tab is raised once the
  strip is complete.

### Two hazards that shaped the implementation

*Saving during the restore would destroy what is being restored.* Restoring is
incremental — the first tab exists before the others — so a save in the middle
records "this app has one tab". An app mid-restore is therefore held in
`RESTORING` and not written until its strip is complete. Without this, a crash
during restore would make the partial state permanent, which is the exact
outcome the feature exists to prevent.

*A popup before the active tab would shift the index.* The active tab is stored
as an index among the tabs that are *kept*, not among all windows. Counting it
against the full list lands on the wrong tab whenever a popup sits earlier in it,
and would restore the user onto a page they were not reading. Covered by
`a_popup_before_the_active_tab_does_not_shift_the_index`, which fails if the index
is computed the naive way.

## Alternatives rejected

- **Store tabs inside `App` in `config.json`.** Loses the corruption isolation,
  rewrites the workspace on every navigation, and mixes run state into the file a
  user is most likely to edit.
- **Save only on quit.** Loses everything on a crash or End Task, which is the
  common case for "the app was closed" on Windows.
- **Store tab ids.** Ids are minted per session (`t2`, `t3`) and a restored tab is
  a new window with a new id, so a stored id names a tab that will not exist.
- **Restore popups too.** Puts back a window the user dismissed. Their URLs are
  not lost: the page that opened them still has them.
- **Restore the first tab always from the session.** Makes editing an app's URL in
  Settings appear to do nothing, because the old address comes back on the next
  launch.

## Consequences

- An app that is opened after a restart comes back with the tabs and pages it had,
  on the tab it was left on.
- Startup preloading (`warm_start`) restores as well, hidden, so the first click
  on an app shows what the user left rather than a cold start page.
- `session.json` grows with the number of apps but stays small: one entry with a
  URL and a title per open tab.
- An app deleted from the workspace keeps its session entry until it is closed
  once. That entry is never restored, because restoring requires the app to
  exist; it is pruned by `session::forget` when the app's last tab closes.
- A crash mid-restore no longer truncates the memory (see `RESTORING`), so a
  restore that fails partway is retried in full on the next launch.

## Verification

- `cargo test --bins` in `src-tauri`: 231 pass. The session tests cover the round
  trip, an app being forgotten when its last tab closes, a corrupt and a
  truncated file, a file missing the newer optional fields, an out-of-range active
  index, out-of-order positions, an edited start page replacing a remembered one,
  a navigated first tab not being erased by that edit, and a tab with no URL
  falling back to the start page.
- `tabs::restore_tests` covers the popup filter, the index shift a popup would
  otherwise cause, and the plain pinned-plus-sub-tab case. The index test was
  verified non-vacuous: computing the index against all tabs instead of the kept
  ones makes it fail.
- Manual end-to-end on Windows: open two sub-tabs (`example.com`, `example.org`)
  in the WhatsApp app, kill the process with `Stop-Process -Force`, relaunch. Both
  sub-tabs came back at `t2` and `t3` with their remembered titles, and `t3` was
  active, matching the saved `"active": 2`.
- `cargo clippy --bins`: no new warnings.
