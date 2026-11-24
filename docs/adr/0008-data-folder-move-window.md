# ADR 0008: Move the data folder while the user watches

**Status:** Accepted (2026-11-24)

## Context

Settings lets the user keep RustBox's data anywhere, and moving it is a real
operation: a 12 GB folder of profiles, cookie databases and engine DLLs. It
cannot happen while the app is open — every profile and engine holds open
handles on the very files that would be copied, and copying an open SQLite
database yields a torn file. So the request is recorded in `config.json` as
`pending_data_root` and applied at the start of the next launch, before anything
opens a profile.

That part already existed. What did not was any way to *see* it. The move ran
silently inside `data_root::startup()`:

- A move that took three minutes looked like a launch that hung. There was no
  window at all until it finished.
- A move that failed left the app on the old folder with the reason in
  `data-move-error.txt`, which the user would only find by opening Settings and
  reading the pane.
- A move that succeeded said nothing, so "did it work?" was answered by
  guessing.

The user's request was concrete: a restart button, then a progress bar that
tells finished from still-running from failed, with a percentage and the error.
Retry after a failure was welcome if it was cheap.

## Decision

A queued move gets its own startup mode and its own window: the *moving
session*.

### The session, not a pane in the app

The app cannot show its own move, because the app's shell window is itself a
webview whose data lives in the folder being copied. And the process that runs
the move can never become the normal app afterwards: `paths::root` is frozen for
the life of a process on purpose, and this process is pinned elsewhere.

So a launch that finds `pending_data_root` becomes a short-lived process:

1. `move_window::take()` returns the job, and `main` builds this window instead
   of the app (see `main.rs`).
2. `move_window::configure()` rewrites the single declared window to
   `move.html`, 560x300, fixed size, centred, instead of the maximized shell —
   and points Tauri's `app_directories_override` at a scratch directory.
3. `move_window::run()` freezes `paths` at that scratch directory
   (`paths::init_root`) and starts the copy on a background thread.
4. Every ending — moved, cancelled, failed — ends in `app.restart()`. The new
   process finds no queued move and starts the real app on the new folder.

The scratch directory (`%TEMP%\rustbox-move-session`) is what makes the session
safe: its WebView2 profile, cache and any path the shell might resolve live
outside both data folders. A profile inside the tree being copied would hold
handles on it for the duration.

`move_window::cleanup()` removes that directory on every launch that is not a
move, since a killed session leaves it behind and WebView2 refuses a profile
that a live browser is using.

### Progress is polled, not pushed

The page asks `move_status` every 250 ms while the copy runs, and stops as soon
as the state leaves `running`. Polling was chosen over emitting events for two
reasons:

- The copy reports progress thousands of times. A request four times a second is
  free next to the seconds a multi-gigabyte copy takes.
- Polling cannot lose the last message. An event listener that has not yet
  registered misses the one event that says "finished", and the window would sit
  on the last percentage forever. The status is a single struct behind a mutex,
  so what the page reads is always internally consistent.

### The outcome has to survive the restart

The moving session cannot report its own result — it ends by restarting. So
`data_root::run` writes `data-move-result.json` into the *locator directory*
(the folder that already holds `data-root.txt` and is therefore the one place
that is readable before the root is known to the next launch). The fields are
`ok`, `cancelled`, `from`, `to`, `files`, `bytes`, `seconds`, `warning` and
`error`.

The next launch reads it once in `reportDataMove()` and shows it as a strip over
the app:

- success: file count, size, duration and the two folders, with a **Dismiss**;
- cancelled: "nothing was copied, the data is still in …";
- failure: the reason, the fact that nothing was copied, and where the data
  still is, with a **Dismiss**.

Dismissing deletes the file. A report that has to be closed on every launch
would turn a one-time answer into a recurring nag.

A move that is merely *queued* is not reported here: the next launch boots the
moving window instead of the shell, so a "there is a move waiting" strip on this
page would be a branch nothing can reach. The queue is reported where it is
useful: Settings names it and offers **Restart now and move**.
The failed record names the *requested* target, not the folder the app stayed
on. A record saying "moved from X to X" would tell the user their data went
somewhere it never went.

### Cancel is not a failure

Pressing **Cancel**, or closing the window, sets a flag the copy checks between
files. The partial copy is rolled back, the pending request is cleared, and the
old root stays authoritative. `End::Cancelled` is reported as its own state and
is not written as an error: nothing went wrong, and a note saying it did would
outlive the decision. The window offers **Try again**, because a cancel is often
"wrong folder" rather than "never mind".

Closing the window mid-copy is turned into the same cancel (`prevent_close` in
the run loop) instead of killing the process with a half-copied target. A close
that arrives while nothing is running goes through.

### Transactional order, unchanged

The ordering predates this ADR and is what makes a crash at any point safe:

1. copy the tree to the target and verify every file arrived at its full length;
2. save the config in the target with the request cleared;
3. write the locator — the one file that makes the target authoritative;
4. delete the old folder, best effort.

Any failure after step 1 rolls the target back to empty, so there is never a
half-copied folder that both sides could claim. A failure in step 4 does not
undo the move: it is reported as a `warning` on a successful `ok: true` record.

The order matters for the record too: a failed copy records `to` as the
requested target, which is exactly what the user typed and where they need to
look if they want to retry.

### Two marker files that make retrying possible

- `data-move-running.json` is written before the copy and removed after it. A
  later launch reads it to tell *its own* interrupted copy apart from a folder
  someone else put files into: the former can be cleared and retried, the latter
  must be refused ("already contains files").
- `free_for_use()` skips all bookkeeping files — the locator, the error note,
  the result and the running marker — so a folder holding only those counts as
  empty.

`remove_old_root()` keeps the old folder's own directory when the locator lives
inside it. Deleting it outright would take the locator with it, and the next
launch would lose the only pointer to the new folder.

### A debugging port, in debug builds only

The move window is the one surface a user cannot click past to reach a console,
so a blank or frozen page there has nothing to read. `RB_MOVE_DEBUG_PORT` opens
a remote-debugging port for this window, and only when `debug_assertions` is on.
A shipped binary never opens one.

The two engines take the port in different places, which is why
`engine::apply` grew a parameter: WebView2 accepts it per window, in that
window's `additional_browser_args`, while Chromium's debugging server belongs to
the browser process and can only be set on the runtime
(`Cef::remote_debugging`). Every other window passes `None`.

## Alternatives rejected

- **A native splash or a Win32 progress bar.** Two UI stacks for one operation,
  and no way to show the error text, the byte count or a Retry button without
  re-implementing text layout. It also cannot be styled with the rest of the app.
- **Emit progress events instead of polling.** The final event can be missed by
  a listener that registers late, and the payload is thousands of messages where
  a snapshot four times a second is enough.
- **Show the move inside the normal app window.** The window is a webview whose
  data is in the tree being copied; the move would fight its own UI for handles.
- **Run the move in a separate helper executable.** A second binary to ship, sign
  and audit, and it would still need to hand off to a restart.
- **Record the result in `config.json`.** The result has to be readable before
  the config is known to be trustworthy (it may sit in the folder that was just
  replaced), and the locator directory is the only location that is always
  readable first.
- **Retry automatically on the next launch.** A failure the user has not seen is
  a failure they cannot fix, and a move that fails for a reason the user controls
  (no space, wrong folder) would loop.

## Consequences

- A queued move now costs one extra process and one extra window, and the app
  cannot reach the new folder without a restart. That is already true — the old
  folder must be released before it can be copied — so making it visible costs
  nothing that was not already being paid.
- The result file is the only cross-launch state; once dismissed, the move is
  forgotten. That is deliberate: the current state is always readable from
  Settings' data-folder pane.
- Two new root-level files (`data-move-result.json`, `data-move-running.json`)
  join `data-root.txt` and `data-move-error.txt`. Anything that enumerates the
  data folder has to skip them, which `free_for_use` does.

## Verification

- `cargo test` in `src-tauri`: 313 pass (26 in `data_root`), plus 5 for the MCP
  shim. The move tests cover the transactional order, cancellation, rollback of
  a failed copy, an interrupted copy being undone instead of blocking the retry,
  a marker from a different move never deleting that folder, a target that is
  inside or above the current root, a target with files, a drive without room,
  the 2 GiB headroom, and the result record every ending produces.
- `cargo check --offline` clean for both the WebView2 and the CEF build.
- `node scripts/check-ui.js` asserts the wiring that a screenshot cannot: that
  `move.html` exists, that it is shown before `data_root::startup()`, that every
  command it invokes is declared and registered, and that both restart entry
  points exist.
- Manual end-to-end on Windows, driven over CDP (`RB_MOVE_DEBUG_PORT`):
  - a 1.8 GB fixture showed `Moving your data folder` / `Copying` with a
    percentage that advanced, then `Data folder moved` / `100%` with the file
    count silently correct;
  - **Cancel** produced `Move cancelled`, an empty target, and an **Open
    RustBox** / **Try again** pair; **Try again** re-ran the copy to completion;
  - a target inside the source produced `The move did not happen` with the exact
    backend error and recorded `ok: false` with the requested target;
  - **Open RustBox** restarted into a new process titled `RustBox`, with the
    locator now pointing at the new folder and the old folder reduced to the
    locator and the result file;
  - a fresh launch then reported the result as a strip with a working
    **Dismiss**.
- The three strip variants and the four window states were also rendered from
  fixtures in headless Chrome, so the copy was reviewed without a real move.
