//! Carrying an app's session across an engine switch.
//!
//! Switching between the OS webview and Chromium changes which directory holds
//! the app's profile, so the session does not follow on its own: the user lands
//! on a login page. This module copies it across, once, at the start of the
//! first launch on the new engine.
//!
//! Two different things have to move, and they fail differently:
//!
//! * **Cookies** are encrypted with a key from each engine's own `Local State`,
//!   so they are decrypted and re-encrypted rather than copied.
//! * **Site storage** (`IndexedDB`, `Local Storage`) is not encrypted at all, so
//!   it is copied as files. This is what a chat app keeps its device pairing in,
//!   and without it a cookie-only migration leaves WhatsApp asking for a QR code
//!   while Gmail, which lives on cookies, comes across signed in.
//!
//! Why it has to be at startup, and why it is a copy rather than a move: a
//! running browser holds the cookie database open, and copying an open SQLite
//! file gives a torn copy. `data_root::startup` already established the one
//! moment where nothing is open; this runs immediately after it, still before
//! any webview exists.
//!
//! # Why the cookies cannot simply be copied
//!
//! Both engines store the same format - `v10` + 12-byte nonce + AES-256-GCM over
//! `SHA-256(host_key) || value` - but each encrypts with a key from its own
//! `Local State`, wrapped with DPAPI under the current Windows user. A cookie
//! copied without its key is unreadable, and Chromium does not leave unreadable
//! rows alone: measured, it deletes them on load (1 row in, 0 rows out). A plain
//! file copy is therefore destructive, not merely useless.
//!
//! So every row is decrypted with the source engine's key and re-encrypted with
//! the target's. Measured: the key is the 32 bytes left after stripping the
//! 5-byte `DPAPI` prefix from `os_crypt.encrypted_key` and unprotecting it, and
//! re-wrapping a key under a different DPAPI description (`Microsoft Edge` vs
//! `Chromium`) yields the same bytes - the description is a label, not an access
//! condition.
//!
//! Rows the source cannot decrypt (an unknown prefix, a cookie written by a
//! different Windows user) are skipped and counted rather than copied blind,
//! because copying them would only give Chromium something to delete.

use crate::config::{Config, Engine, Runtime};
use std::path::{Path, PathBuf};

/// Where a failed migration leaves its reason for the settings page to show.
pub const ERROR_FILE: &str = "engine-migrate-error.txt";

/// Cookie columns both engines have. WebView2 adds `is_edgelegacycookie` and
/// `browser_provenance` on top, both `DEFAULT 0`, so naming these twenty works
/// against either schema and lets the target's own defaults fill the rest.
const COLUMNS: [&str; 20] = [
    "creation_utc",
    "host_key",
    "top_frame_site_key",
    "name",
    "value",
    "encrypted_value",
    "path",
    "expires_utc",
    "is_secure",
    "is_httponly",
    "last_access_utc",
    "has_expires",
    "is_persistent",
    "priority",
    "samesite",
    "source_scheme",
    "source_port",
    "last_update_utc",
    "source_type",
    "has_cross_site_ancestor",
];

/// One row of a cookie jar, decoupled from the connection it came from.
struct Row {
    values: Vec<rusqlite::types::Value>,
}

/// What the copy did, for the log line and the settings page.
#[derive(Default)]
pub struct Report {
    /// Apps whose session was carried over.
    pub apps: usize,
    /// Cookie rows re-encrypted into the target.
    pub copied: usize,
    /// Rows the source key could not open, so they were left behind.
    pub unreadable: usize,
    /// Site-storage directories copied (IndexedDB, Local Storage).
    pub stores: usize,
    /// Bytes those directories hold, so the cost is not a surprise.
    pub store_bytes: u64,
    /// Apps whose site storage was left alone because the target already had
    /// some. A profile that is in use is never overwritten.
    pub stores_kept: usize,
    /// Apps that could not be migrated, with the reason.
    pub failures: Vec<String>,
}

impl Report {
    fn describe(&self, from: Runtime, to: Runtime) -> String {
        let mut line = format!(
            "carried {} cookies from {} to {} for {} app(s)",
            self.copied,
            engine_label(from),
            engine_label(to),
            self.apps
        );
        if self.stores > 0 {
            line.push_str(&format!(
                "; plus {} site-storage director{} ({} MB), which is what a chat app keeps its device pairing in",
                self.stores,
                if self.stores == 1 { "y" } else { "ies" },
                self.store_bytes / (1024 * 1024)
            ));
        }
        if self.stores_kept > 0 {
            line.push_str(&format!(
                "; {} app(s) already had site storage on the new engine, so theirs was left untouched",
                self.stores_kept
            ));
        }
        if self.unreadable > 0 {
            line.push_str(&format!(
                "; {} cookie(s) could not be read with the old engine's key and were left behind",
                self.unreadable
            ));
        }
        if !self.failures.is_empty() {
            line.push_str(&format!("; failed: {}", self.failures.join("; ")));
        }
        line
    }
}

fn engine_label(runtime: Runtime) -> &'static str {
    match runtime {
        Runtime::OsWebview => "the OS webview",
        Runtime::Chromium => "Chromium",
    }
}

/// The two files that make up one engine's cookie storage for one app, plus the
/// directory its site storage lives in.
struct Jar {
    cookies: PathBuf,
    local_state: PathBuf,
    /// Root of the app's site storage: `IndexedDB`, `Local Storage`, and the
    /// rest hang off it. `None` when the layout is not known for the mode, so a
    /// missing answer can never be mistaken for "empty".
    site_root: Option<PathBuf>,
}

fn wry_jar(app_id: &str, mode: crate::config::ProfileMode) -> Jar {
    match mode {
        // An `Isolated` profile is a whole WebView2 user-data directory, so the
        // `Local State` that holds its key is the one inside it.
        crate::config::ProfileMode::Isolated => {
            let env = crate::paths::embedded_profile(app_id).join("EBWebView");
            Jar {
                cookies: env.join("Default").join("Network").join("Cookies"),
                local_state: env.join("Local State"),
                site_root: Some(env.join("Default")),
            }
        }
        // A `Light` profile is a named profile inside the shell's own user-data
        // directory, so its cookies live under the profile folder but its key is
        // the shared one.
        crate::config::ProfileMode::Light => Jar {
            cookies: crate::paths::light_profile(app_id)
                .join("Network")
                .join("Cookies"),
            local_state: crate::paths::root().join("EBWebView").join("Local State"),
            site_root: Some(crate::paths::light_profile(app_id)),
        },
    }
}

fn cef_jar(app_id: &str) -> Jar {
    let profile = crate::paths::cef_profile(app_id);
    Jar {
        cookies: profile.join("Network").join("Cookies"),
        // One key for the whole engine root: CEF keeps a single `Local State`,
        // unlike WebView2, which keeps one per user-data directory.
        local_state: crate::paths::cef_root().join("Local State"),
        site_root: Some(profile),
    }
}

fn jar(runtime: Runtime, app_id: &str, mode: crate::config::ProfileMode) -> Jar {
    match runtime {
        Runtime::OsWebview => wry_jar(app_id, mode),
        Runtime::Chromium => cef_jar(app_id),
    }
}

/// Migrate every embedded app if the engine changed since the last launch.
///
/// Returns a note worth logging, or `None` when there was nothing to do. Called
/// from `main` after `data_root::startup`, which is what freezes the data root,
/// and before the hand-off, so exactly one process does the work.
pub fn startup(config_path: &Path) -> Option<String> {
    let mut config = match Config::load(config_path) {
        Ok(c) => c,
        // An unreadable config is reported properly by `AppState::new` a moment
        // later; migrating against a guessed config would be worse than waiting.
        Err(_) => return None,
    };

    // A config written before this field existed was running the OS webview:
    // that was the only engine, so it is what the cookies belong to.
    let previous = config.last_runtime.unwrap_or(Runtime::OsWebview);
    let now = config.runtime;

    let mut notes = Vec::new();
    if previous != now {
        let report = migrate(&config, previous, now);
        if report.copied > 0
            || report.stores > 0
            || report.stores_kept > 0
            || !report.failures.is_empty()
            || report.unreadable > 0
        {
            notes.push(report.describe(previous, now));
        }
        if !report.failures.is_empty() {
            record_error(&report.describe(previous, now));
        }
    }

    // Recorded even when nothing was migrated, so the *next* switch knows which
    // engine the cookies on disk came from.
    if config.last_runtime != Some(now) {
        config.last_runtime = Some(now);
        if let Err(e) = config.save(config_path) {
            notes.push(format!("could not record the engine that ran: {e}"));
        }
    }

    if notes.is_empty() {
        None
    } else {
        Some(notes.join("; "))
    }
}

/// Copy every embedded app's cookies from `from`'s jar to `to`'s, and carry the
/// site storage a chat app keeps its pairing in.
fn migrate(config: &Config, from: Runtime, to: Runtime) -> Report {
    let mut report = Report::default();

    for app in &config.apps {
        // Only the embedded engines keep a cookie jar we can reach; an app that
        // runs in an external browser has its own profile elsewhere.
        if app.engine != Engine::OsWebview {
            continue;
        }
        let source = jar(from, &app.id, app.profile_mode);
        let target = jar(to, &app.id, app.profile_mode);
        if source.cookies.exists() {
            match copy_app(&source, &target, to == Runtime::OsWebview) {
                Ok((copied, unreadable)) => {
                    report.apps += 1;
                    report.copied += copied;
                    report.unreadable += unreadable;
                }
                Err(e) => report.failures.push(format!("{}: {e}", app.name)),
            }
        }
        // Independent of the cookies: an app whose jar is gone can still have
        // the storage that holds its login.
        match carry_site_storage(&source, &target) {
            Ok(Carried::Copied { dirs, bytes }) => {
                report.stores += dirs;
                report.store_bytes += bytes;
            }
            Ok(Carried::TargetInUse) => report.stores_kept += 1,
            Ok(Carried::Nothing) => {}
            Err(e) => report
                .failures
                .push(format!("{} site storage: {e}", app.name)),
        }
    }

    report
}

/// What carrying one app's site storage did.
enum Carried {
    /// Directories copied and the bytes they hold.
    Copied { dirs: usize, bytes: u64 },
    /// The destination already had storage of its own, so nothing was touched.
    TargetInUse,
    /// The source had nothing to carry.
    Nothing,
}

/// The directories a site keeps its session in, and nothing else.
///
/// Deliberately short, and only what was measured. `Cache`, `Code Cache`,
/// `GPUCache`, `Service Worker` and `Shared Dictionary` are derived data:
/// copying them costs hundreds of megabytes and buys nothing, because the
/// browser rebuilds them on the next load. Measured on this machine, one
/// WhatsApp profile was 386 MB of IndexedDB plus **518 MB of Service Worker
/// cache** — the second half is what makes a naive "copy the whole profile"
/// approach feel like a mistake. `Session Storage` is cleared when the browser
/// closes, so it carries nothing across a restart.
const SITE_STORES: [&str; 2] = [
    // IndexedDB and its blob side table are separate directories: the leveldb
    // holds the records, the blob store holds the bytes they point at, so a copy
    // of one without the other is a database pointing at nothing.
    "IndexedDB",
    "Local Storage",
];

/// Copy an app's site storage into the incoming engine, unless it has a session
/// of its own there.
///
/// # Why this is safe
///
/// These stores are **not** encrypted per engine, unlike the cookie jar: they are
/// LevelDB the browser reads as-is. Measured across WebView2 157 and CEF 154 —
/// different Chromium milestones and different keys — one WhatsApp profile
/// carried its `IndexedDB` across and came up signed in (`qr: false`,
/// `pane: true`, 81 unread), while a fresh profile asked for a QR code. The
/// database names and versions the site checks (`model-storage` v217) are the
/// site's own, so they agree whenever the site is the same site.
///
/// # Why it refuses to overwrite
///
/// A target that already holds storage belongs to a session the user has been
/// using on the new engine. Replacing it would sign them *out* to fix a problem
/// they may no longer have, so a copy only fills what is empty.
fn carry_site_storage(source: &Jar, target: &Jar) -> Result<Carried, String> {
    let (Some(from), Some(to)) = (&source.site_root, &target.site_root) else {
        return Ok(Carried::Nothing);
    };
    // Same tree on both sides cannot happen (the two engines never share a
    // profile root), but copying a directory onto itself is not something to
    // find out the hard way.
    if from == to {
        return Ok(Carried::Nothing);
    }
    let mut dirs = 0;
    let mut bytes = 0;
    let mut kept = 0;
    for name in SITE_STORES {
        let src = from.join(name);
        if !src.is_dir() {
            continue;
        }
        let src_bytes = tree_bytes(&src);
        if src_bytes <= MIN_STORE_BYTES {
            continue;
        }
        let dst = to.join(name);
        if worth_keeping(tree_bytes(&dst), src_bytes) {
            kept += 1;
            continue;
        }
        // A stub the site created when it found nothing is replaced, not merged:
        // two leveldbs cannot be merged by copying files, and the stub is the
        // thing that made the user see a QR code.
        if dst.exists() {
            std::fs::remove_dir_all(&dst)
                .map_err(|e| format!("could not clear {}: {e}", dst.display()))?;
        }
        copy_tree(&src, &dst)?;
        dirs += 1;
        bytes += tree_bytes(&dst);
    }
    Ok(match (dirs, kept) {
        (0, 0) => Carried::Nothing,
        (0, _) => Carried::TargetInUse,
        _ => Carried::Copied { dirs, bytes },
    })
}

/// Whether a target store already holds something worth protecting.
///
/// Size, not existence: Chromium creates these directories on first launch and
/// writes a `CURRENT`, a `LOCK` and a `MANIFEST` before the site has stored
/// anything, and a site that found no session writes a small database of its own
/// (measured: 449 KB for WhatsApp) while the real one is hundreds of megabytes.
/// A quarter of the source is the line between "a session someone has been
/// using" and "a stub", and it is deliberately generous to the target: a real
/// session is never a small fraction of the one it replaced.
fn worth_keeping(target_bytes: u64, source_bytes: u64) -> bool {
    target_bytes > MIN_STORE_BYTES && target_bytes * 4 >= source_bytes
}

/// Below this, a store directory holds Chromium's own scaffolding (a `CURRENT`,
/// a `LOCK`, an empty `MANIFEST`) rather than a session.
const MIN_STORE_BYTES: u64 = 16 * 1024;

fn tree_bytes(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut total = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        match entry.file_type() {
            Ok(t) if t.is_dir() => total += tree_bytes(&path),
            Ok(t) if t.is_file() => total += entry.metadata().map(|m| m.len()).unwrap_or(0),
            _ => {}
        }
    }
    total
}

fn copy_tree(src: &Path, dst: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dst)
        .map_err(|e| format!("could not create {}: {e}", dst.display()))?;
    let entries =
        std::fs::read_dir(src).map_err(|e| format!("could not read {}: {e}", src.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("could not read {}: {e}", src.display()))?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let kind = entry
            .file_type()
            .map_err(|e| format!("could not stat {}: {e}", from.display()))?;
        if kind.is_dir() {
            copy_tree(&from, &to)?;
        } else if kind.is_file() {
            std::fs::copy(&from, &to).map_err(|e| {
                format!(
                    "could not copy {} to {}: {e}",
                    from.display(),
                    to.display()
                )
            })?;
        }
        // Anything else (a junction, a device node) is not part of a LevelDB and
        // is skipped rather than followed.
    }
    Ok(())
}

/// Move one app's rows from `source` to `target`.
///
/// Returns rows copied and rows that could not be decrypted. The target's own
/// cookies are kept: a row that exists in both jars is replaced by the source's,
/// because the source is the engine the user has actually been using.
fn copy_app(source: &Jar, target: &Jar, edge_schema: bool) -> Result<(usize, usize), String> {
    let source_key = read_key(&source.local_state)?.ok_or_else(|| {
        format!(
            "no cookie key in {}: the old engine never finished starting",
            source.local_state.display()
        )
    })?;
    let rows = read_rows(&source.cookies)?;

    // The target's key is created when it does not have one yet, which is the
    // case on the first switch to an engine. Writing it here is the same thing
    // Chromium does on its own first run, and it is what makes the re-encrypted
    // rows readable by the engine that is about to start.
    let target_key = match read_key(&target.local_state)? {
        Some(key) => key,
        None => {
            let key = crypto::random_key()?;
            write_key(&target.local_state, &key)?;
            key
        }
    };

    if !target.cookies.exists() {
        copy_jar_file(&source.cookies, &target.cookies)?;
        if edge_schema {
            // A jar copied from the Chromium engine has twenty columns; WebView2
            // expects its two extra ones to exist. They default to 0, which is
            // what WebView2 writes for a cookie it did not see itself.
            add_edge_columns(&target.cookies)?;
        }
        // Every row in the copy is the source's, encrypted with the source's
        // key. Clearing them first means a row this run cannot decrypt is left
        // absent instead of present-and-unreadable, which is the state that
        // makes Chromium delete things.
        let conn = open(&target.cookies)?;
        conn.execute("DELETE FROM cookies", [])
            .map_err(|e| format!("could not clear {}: {e}", target.cookies.display()))?;
    }

    let source_key = crypto::CookieKey::new(source_key)?;
    let target_key = crypto::CookieKey::new(target_key)?;

    let mut copied = 0;
    let mut unreadable = 0;
    let conn = open(&target.cookies)?;
    for row in rows {
        match reencrypt(&row, &source_key, &target_key) {
            Ok(row) => {
                insert(&conn, &row)?;
                copied += 1;
            }
            Err(_) => unreadable += 1,
        }
    }
    Ok((copied, unreadable))
}

/// Rewrite one row's `encrypted_value` under a different key.
fn reencrypt(
    row: &Row,
    source_key: &crypto::CookieKey,
    target_key: &crypto::CookieKey,
) -> Result<Row, String> {
    let mut values = row.values.clone();
    let host_key = match &values[1] {
        rusqlite::types::Value::Text(t) => t.clone(),
        _ => return Err("host_key is not text".to_string()),
    };
    let blob = match &values[5] {
        rusqlite::types::Value::Blob(b) => b.clone(),
        // A cookie stored unencrypted (`value` set, `encrypted_value` empty) is
        // readable by both engines as it stands, so it needs no rewriting.
        _ => return Ok(Row { values }),
    };
    if blob.is_empty() {
        return Ok(Row { values });
    }

    let value = source_key.decrypt(&blob, &host_key)?;
    let rewritten = target_key.encrypt(&value, &host_key)?;
    values[4] = rusqlite::types::Value::Text(String::new());
    values[5] = rusqlite::types::Value::Blob(rewritten);
    Ok(Row { values })
}

fn read_rows(path: &Path) -> Result<Vec<Row>, String> {
    // `Connection::open` creates a missing file, so a path that vanished between
    // the existence check and here would turn into an empty jar the migration
    // then "succeeds" on.
    if !path.exists() {
        return Err(format!("{} does not exist", path.display()));
    }
    let conn = open(path)?;
    let sql = format!("SELECT {} FROM cookies", COLUMNS.join(", "));
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| format!("could not read {}: {e}", path.display()))?;
    let rows = stmt
        .query_map([], |row| {
            let mut values = Vec::with_capacity(COLUMNS.len());
            for i in 0..COLUMNS.len() {
                values.push(row.get::<_, rusqlite::types::Value>(i)?);
            }
            Ok(Row { values })
        })
        .map_err(|e| format!("could not read {}: {e}", path.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("could not read {}: {e}", path.display()))?;
    Ok(rows)
}

fn insert(conn: &rusqlite::Connection, row: &Row) -> Result<(), String> {
    let placeholders: Vec<String> = (1..=COLUMNS.len()).map(|i| format!("?{i}")).collect();
    let sql = format!(
        "INSERT OR REPLACE INTO cookies ({}) VALUES ({})",
        COLUMNS.join(", "),
        placeholders.join(", ")
    );
    conn.execute(&sql, rusqlite::params_from_iter(row.values.iter()))
        .map_err(|e| format!("could not write a cookie: {e}"))?;
    Ok(())
}

fn open(path: &Path) -> Result<rusqlite::Connection, String> {
    rusqlite::Connection::open(path).map_err(|e| format!("could not open {}: {e}", path.display()))
}

fn copy_jar_file(from: &Path, to: &Path) -> Result<(), String> {
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("could not create {}: {e}", parent.display()))?;
    }
    // A `-wal` left beside the destination by an earlier crash would be replayed
    // into the copy and resurrect rows this run deliberately dropped.
    for suffix in ["-wal", "-shm"] {
        let side = sidecar(to, suffix);
        let _ = std::fs::remove_file(side);
    }
    std::fs::copy(from, to).map_err(|e| {
        format!(
            "could not copy {} to {}: {e}",
            from.display(),
            to.display()
        )
    })?;
    Ok(())
}

fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

/// Give a jar copied from Chromium the shape WebView2 expects.
///
/// Two columns and two `meta` rows. Measured against a jar WebView2 built for
/// itself on a clean profile: it writes `edge_version_number` and
/// `compatible_edge_version_number` alongside the generic version keys, and a
/// jar without them is not adopted - WebView2 replaced the whole file with an
/// empty one instead of reading it.
fn add_edge_columns(path: &Path) -> Result<(), String> {
    let conn = open(path)?;
    let mut stmt = conn
        .prepare("PRAGMA table_info(cookies)")
        .map_err(|e| format!("could not read {}: {e}", path.display()))?;
    let names: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|e| format!("could not read {}: {e}", path.display()))?
        .filter_map(Result::ok)
        .collect();
    for column in ["is_edgelegacycookie", "browser_provenance"] {
        if !names.iter().any(|n| n == column) {
            conn.execute_batch(&format!(
                "ALTER TABLE cookies ADD COLUMN {column} INTEGER DEFAULT 0"
            ))
            .map_err(|e| format!("could not extend {}: {e}", path.display()))?;
        }
    }
    for (key, value) in [
        ("edge_version_number", "3"),
        ("compatible_edge_version_number", "3"),
    ] {
        conn.execute(
            "INSERT OR IGNORE INTO meta (key, value) VALUES (?1, ?2)",
            rusqlite::params![key, value],
        )
        .map_err(|e| format!("could not extend {}: {e}", path.display()))?;
    }
    Ok(())
}

mod crypto {
    //! The two primitives Chromium's cookie stack needs on Windows: DPAPI for
    //! the key, and AES-256-GCM for the values.
    //!
    //! AES comes from Windows' own CNG (`bcrypt.dll`) rather than a crate: this
    //! build has no AES dependency available, and the C API here is a documented
    //! one-call-per-operation cipher rather than a mode this code has to get
    //! right itself.

    use sha2::{Digest, Sha256};
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        BCRYPT_AES_ALGORITHM, BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO,
        BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO_VERSION, BCRYPT_CHAINING_MODE,
        BCRYPT_OBJECT_LENGTH, BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptCloseAlgorithmProvider,
        BCryptDecrypt, BCryptDestroyKey, BCryptEncrypt, BCryptGenRandom,
        BCryptGenerateSymmetricKey, BCryptGetProperty, BCryptOpenAlgorithmProvider,
        BCryptSetProperty, CRYPT_INTEGER_BLOB, CryptProtectData, CryptUnprotectData,
    };

    use windows_sys::Win32::Security::Cryptography::{BCRYPT_ALG_HANDLE, BCRYPT_KEY_HANDLE};

    const TAG_LEN: usize = 16;
    const NONCE_LEN: usize = 12;

    /// The `DPAPI` prefix Chromium puts in front of the wrapped key.
    const DPAPI: &[u8] = b"DPAPI";

    fn nt(status: i32, what: &str) -> Result<(), String> {
        if status < 0 {
            Err(format!("{what} failed (NTSTATUS 0x{status:08X})"))
        } else {
            Ok(())
        }
    }

    /// A 32-byte key with the AES provider already open for GCM.
    ///
    /// The provider handle is per key because the key handle has to be rebuilt
    /// for each operation, and that wants the algorithm handle to stay alive.
    pub struct CookieKey {
        alg: BCRYPT_ALG_HANDLE,
        key: [u8; 32],
    }

    impl Drop for CookieKey {
        fn drop(&mut self) {
            unsafe { BCryptCloseAlgorithmProvider(self.alg, 0) };
        }
    }

    impl CookieKey {
        pub fn new(key: [u8; 32]) -> Result<Self, String> {
            let mut alg: BCRYPT_ALG_HANDLE = std::ptr::null_mut();
            unsafe { nt(BCryptOpenAlgorithmProvider(&mut alg, BCRYPT_AES_ALGORITHM, std::ptr::null(), 0), "opening the AES provider")? };

            // GCM is a chaining mode CNG has to be told about before a key is
            // generated; the value is a UTF-16 string including its terminator.
            let mode: Vec<u16> = "ChainingModeGCM\0".encode_utf16().collect();
            let status = unsafe {
                BCryptSetProperty(
                    alg,
                    BCRYPT_CHAINING_MODE,
                    mode.as_ptr() as *const u8,
                    (mode.len() * 2) as u32,
                    0,
                )
            };
            if let Err(e) = nt(status, "selecting GCM") {
                unsafe { BCryptCloseAlgorithmProvider(alg, 0) };
                return Err(e);
            }
            Ok(Self { alg, key })
        }

        /// Run `f` with a key handle, destroying it whatever `f` decides.
        fn with_key<T>(
            &self,
            f: impl FnOnce(BCRYPT_KEY_HANDLE) -> Result<T, String>,
        ) -> Result<T, String> {
            let mut object_len: u32 = 0;
            let mut written: u32 = 0;
            unsafe {
                nt(
                    BCryptGetProperty(
                        self.alg,
                        BCRYPT_OBJECT_LENGTH,
                        &mut object_len as *mut u32 as *mut u8,
                        std::mem::size_of::<u32>() as u32,
                        &mut written,
                        0,
                    ),
                    "reading the key object size",
                )?
            };
            let mut object = vec![0u8; object_len as usize];
            let mut handle: BCRYPT_KEY_HANDLE = std::ptr::null_mut();
            unsafe {
                nt(
                    BCryptGenerateSymmetricKey(
                        self.alg,
                        &mut handle,
                        object.as_mut_ptr(),
                        object_len,
                        self.key.as_ptr(),
                        self.key.len() as u32,
                        0,
                    ),
                    "loading the cookie key",
                )?
            };
            let out = f(handle);
            unsafe { BCryptDestroyKey(handle) };
            out
        }

        /// Encrypt one cookie value into the `v10` blob Chromium stores.
        ///
        /// The plaintext is `SHA-256(host_key) || value`, which binds the blob to
        /// the host it belongs to: moving a blob to another host's row makes it
        /// fail authentication.
        pub fn encrypt(&self, value: &str, host_key: &str) -> Result<Vec<u8>, String> {
            let mut plain = Vec::with_capacity(32 + value.len());
            plain.extend_from_slice(&Sha256::digest(host_key.as_bytes()));
            plain.extend_from_slice(value.as_bytes());
            let nonce = random_nonce()?;

            self.with_key(|handle| {
                let mut tag = [0u8; TAG_LEN];
                let mut info = BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO {
                    cbSize: std::mem::size_of::<BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO>() as u32,
                    dwInfoVersion: BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO_VERSION,
                    pbNonce: nonce.as_ptr() as *mut u8,
                    cbNonce: NONCE_LEN as u32,
                    pbAuthData: std::ptr::null_mut(),
                    cbAuthData: 0,
                    pbTag: tag.as_mut_ptr(),
                    cbTag: TAG_LEN as u32,
                    pbMacContext: std::ptr::null_mut(),
                    cbMacContext: 0,
                    cbAAD: 0,
                    cbData: plain.len() as u64,
                    dwFlags: 0,
                };
                let mut out = vec![0u8; plain.len()];
                let mut written = 0u32;
                let status = unsafe {
                    BCryptEncrypt(
                        handle,
                        plain.as_ptr(),
                        plain.len() as u32,
                        &mut info as *mut _ as *const core::ffi::c_void,
                        std::ptr::null_mut(),
                        0,
                        out.as_mut_ptr(),
                        out.len() as u32,
                        &mut written,
                        0,
                    )
                };
                nt(status, "encrypting a cookie")?;
                out.truncate(written as usize);

                let mut blob = Vec::with_capacity(3 + NONCE_LEN + out.len() + TAG_LEN);
                blob.extend_from_slice(b"v10");
                blob.extend_from_slice(&nonce);
                blob.extend_from_slice(&out);
                blob.extend_from_slice(&tag);
                Ok(blob)
            })
        }

        /// Open a `v10` blob written by any engine using this key.
        pub fn decrypt(&self, blob: &[u8], host_key: &str) -> Result<String, String> {
            if blob.len() < 3 + NONCE_LEN + TAG_LEN || &blob[..3] != b"v10" {
                return Err("not a v10 cookie".to_string());
            }
            let nonce = &blob[3..3 + NONCE_LEN];
            let cipher = &blob[3 + NONCE_LEN..blob.len() - TAG_LEN];
            let mut tag = blob[blob.len() - TAG_LEN..].to_vec();

            self.with_key(|handle| {
                let mut info = BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO {
                    cbSize: std::mem::size_of::<BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO>() as u32,
                    dwInfoVersion: BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO_VERSION,
                    pbNonce: nonce.as_ptr() as *mut u8,
                    cbNonce: NONCE_LEN as u32,
                    pbAuthData: std::ptr::null_mut(),
                    cbAuthData: 0,
                    pbTag: tag.as_mut_ptr(),
                    cbTag: TAG_LEN as u32,
                    pbMacContext: std::ptr::null_mut(),
                    cbMacContext: 0,
                    cbAAD: 0,
                    cbData: 0,
                    dwFlags: 0,
                };
                let mut out = vec![0u8; cipher.len()];
                let mut written = 0u32;
                let status = unsafe {
                    BCryptDecrypt(
                        handle,
                        cipher.as_ptr(),
                        cipher.len() as u32,
                        &mut info as *mut _ as *const core::ffi::c_void,
                        std::ptr::null_mut(),
                        0,
                        out.as_mut_ptr(),
                        out.len() as u32,
                        &mut written,
                        0,
                    )
                };
                nt(status, "decrypting a cookie")?;
                out.truncate(written as usize);

                let hash = Sha256::digest(host_key.as_bytes());
                if out.len() < 32 || out[..32] != hash[..] {
                    return Err("cookie is bound to a different host".to_string());
                }
                String::from_utf8(out[32..].to_vec())
                    .map_err(|e| format!("cookie value is not UTF-8: {e}"))
            })
        }
    }

    fn random_nonce() -> Result<[u8; NONCE_LEN], String> {
        let mut nonce = [0u8; NONCE_LEN];
        random_bytes(&mut nonce)?;
        Ok(nonce)
    }

    pub fn random_key() -> Result<[u8; 32], String> {
        let mut key = [0u8; 32];
        random_bytes(&mut key)?;
        Ok(key)
    }

    fn random_bytes(buf: &mut [u8]) -> Result<(), String> {
        let status = unsafe {
            BCryptGenRandom(
                std::ptr::null_mut(),
                buf.as_mut_ptr(),
                buf.len() as u32,
                BCRYPT_USE_SYSTEM_PREFERRED_RNG,
            )
        };
        nt(status, "generating random bytes")
    }

    fn dpapi_unprotect(blob: &[u8]) -> Result<Vec<u8>, String> {
        let input = CRYPT_INTEGER_BLOB {
            cbData: blob.len() as u32,
            pbData: blob.as_ptr() as *mut u8,
        };
        let mut out = CRYPT_INTEGER_BLOB::default();
        let ok = unsafe {
            CryptUnprotectData(
                &input,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                0,
                &mut out,
            )
        };
        if ok == 0 {
            return Err(format!(
                "DPAPI could not unwrap the cookie key: {}",
                std::io::Error::last_os_error()
            ));
        }
        let bytes = unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize) }.to_vec();
        // DPAPI allocates with the COM task allocator's predecessor, `LocalAlloc`.
        unsafe { LocalFree(out.pbData as *mut core::ffi::c_void) };
        Ok(bytes)
    }

    fn dpapi_protect(blob: &[u8]) -> Result<Vec<u8>, String> {
        let input = CRYPT_INTEGER_BLOB {
            cbData: blob.len() as u32,
            pbData: blob.as_ptr() as *mut u8,
        };
        let mut out = CRYPT_INTEGER_BLOB::default();
        // The description is a label only. Measured: a key wrapped as "Microsoft
        // Edge" unwraps identically when it is labelled "Chromium", so binding
        // the copy to either name would be superstition.
        let description: Vec<u16> = "Chromium\0".encode_utf16().collect();
        let ok = unsafe {
            CryptProtectData(
                &input,
                description.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                0,
                &mut out,
            )
        };
        if ok == 0 {
            return Err(format!(
                "DPAPI could not wrap the cookie key: {}",
                std::io::Error::last_os_error()
            ));
        }
        let bytes = unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize) }.to_vec();
        unsafe { LocalFree(out.pbData as *mut core::ffi::c_void) };
        Ok(bytes)
    }

    /// The 32 raw bytes behind `os_crypt.encrypted_key`.
    pub fn unwrap_key(encoded: &str) -> Result<[u8; 32], String> {
        use base64::Engine as _;
        let raw = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|e| format!("the cookie key is not base64: {e}"))?;
        let blob = raw.strip_prefix(DPAPI).unwrap_or(&raw);
        let plain = dpapi_unprotect(blob)?;
        <[u8; 32]>::try_from(plain.as_slice()).map_err(|_| {
            format!(
                "the cookie key is {} bytes, expected 32",
                plain.len()
            )
        })
    }

    /// The value `os_crypt.encrypted_key` has to hold for this key.
    pub fn wrap_key(key: &[u8; 32]) -> Result<String, String> {
        use base64::Engine as _;
        let blob = dpapi_protect(key)?;
        let mut raw = Vec::with_capacity(DPAPI.len() + blob.len());
        raw.extend_from_slice(DPAPI);
        raw.extend_from_slice(&blob);
        Ok(base64::engine::general_purpose::STANDARD.encode(raw))
    }
}

/// The 32-byte AES key from a `Local State`, if the engine has run there yet.
fn read_key(local_state: &Path) -> Result<Option<[u8; 32]>, String> {
    if !local_state.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(local_state)
        .map_err(|e| format!("could not read {}: {e}", local_state.display()))?;
    let json: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        // A half-written `Local State` is the engine's problem to rebuild, not
        // ours to guess at.
        Err(e) => return Err(format!("{} is not valid JSON: {e}", local_state.display())),
    };
    let Some(encoded) = json
        .get("os_crypt")
        .and_then(|c| c.get("encrypted_key"))
        .and_then(|k| k.as_str())
    else {
        return Ok(None);
    };
    let key = crypto::unwrap_key(encoded)?;
    Ok(Some(key))
}

/// Store a key the way Chromium does, preserving every other field.
fn write_key(local_state: &Path, key: &[u8; 32]) -> Result<(), String> {
    let mut json: serde_json::Value = if local_state.exists() {
        let raw = std::fs::read_to_string(local_state)
            .map_err(|e| format!("could not read {}: {e}", local_state.display()))?;
        serde_json::from_str(&raw)
            .map_err(|e| format!("{} is not valid JSON: {e}", local_state.display()))?
    } else {
        serde_json::json!({})
    };
    let object = json
        .as_object_mut()
        .ok_or_else(|| format!("{} is not a JSON object", local_state.display()))?;
    let crypt = object
        .entry("os_crypt")
        .or_insert_with(|| serde_json::json!({}));
    let crypt = crypt
        .as_object_mut()
        .ok_or_else(|| format!("{} has a non-object os_crypt", local_state.display()))?;
    crypt.insert(
        "encrypted_key".to_string(),
        serde_json::Value::String(crypto::wrap_key(key)?),
    );

    if let Some(parent) = local_state.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("could not create {}: {e}", parent.display()))?;
    }
    let text = serde_json::to_string(&json).map_err(|e| e.to_string())?;
    // Temp file + rename: a crash mid-write must not leave the key half-written,
    // because Chromium would then generate a new one and orphan every cookie.
    let tmp = local_state.with_extension("tmp");
    std::fs::write(&tmp, text).map_err(|e| format!("could not write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, local_state).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("could not update {}: {e}", local_state.display())
    })
}

fn error_path() -> PathBuf {
    crate::paths::root().join(ERROR_FILE)
}

fn record_error(reason: &str) {
    let path = error_path();
    let _ = std::fs::write(path, format!("{reason}\n"));
}

/// Read and clear the reason a migration failed, so the settings page can show
/// it once instead of every launch.
pub fn take_error() -> Option<String> {
    let path = error_path();
    let text = std::fs::read_to_string(&path).ok()?;
    let _ = std::fs::remove_file(&path);
    let text = text.trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    /// A scratch directory unique to this test, so two runs cannot collide.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rb-migrate-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A cookie jar with the twenty shared columns, plus WebView2's two when
    /// `edge` is set.
    fn jar_at(path: &Path, edge: bool) -> Connection {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let conn = Connection::open(path).unwrap();
        let mut columns = COLUMNS
            .iter()
            .map(|c| match *c {
                "host_key" | "top_frame_site_key" | "name" | "value" | "path" => {
                    format!("{c} TEXT NOT NULL")
                }
                "encrypted_value" => format!("{c} BLOB NOT NULL"),
                other => format!("{other} INTEGER NOT NULL"),
            })
            .collect::<Vec<_>>();
        if edge {
            columns.push("is_edgelegacycookie INTEGER DEFAULT 0".to_string());
            columns.push("browser_provenance INTEGER DEFAULT 0".to_string());
        }
        conn.execute_batch(&format!(
            "CREATE TABLE meta(key LONGVARCHAR NOT NULL UNIQUE PRIMARY KEY, value LONGVARCHAR);
             CREATE TABLE cookies({});
             CREATE UNIQUE INDEX cookies_unique_index ON cookies(host_key, top_frame_site_key,
                has_cross_site_ancestor, name, path, source_scheme, source_port);",
            columns.join(", ")
        ))
        .unwrap();
        conn
    }

    fn put(conn: &Connection, host: &str, name: &str, blob: Vec<u8>) {
        let placeholders = (1..=COLUMNS.len())
            .map(|i| format!("?{i}"))
            .collect::<Vec<_>>()
            .join(", ");
        let mut values: Vec<rusqlite::types::Value> = vec![
            1.into(),
            host.to_string().into(),
            "".to_string().into(),
            name.to_string().into(),
            "".to_string().into(),
            blob.into(),
            "/".to_string().into(),
        ];
        for _ in 7..COLUMNS.len() {
            values.push(0i64.into());
        }
        conn.execute(
            &format!(
                "INSERT INTO cookies ({}) VALUES ({placeholders})",
                COLUMNS.join(", ")
            ),
            rusqlite::params_from_iter(values.iter()),
        )
        .unwrap();
    }

    fn blob_of(conn: &Connection, name: &str) -> Vec<u8> {
        conn.query_row(
            "SELECT encrypted_value FROM cookies WHERE name = ?1",
            [name],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .unwrap()
    }

    #[test]
    fn a_wrapped_key_survives_the_round_trip() {
        let key = [7u8; 32];
        let encoded = crypto::wrap_key(&key).unwrap();
        // Chromium's on-disk form: the literal `DPAPI` then the protected blob.
        let raw = {
            use base64::Engine as _;
            base64::engine::general_purpose::STANDARD
                .decode(&encoded)
                .unwrap()
        };
        assert_eq!(&raw[..5], b"DPAPI", "the prefix Chromium writes");
        assert_eq!(crypto::unwrap_key(&encoded).unwrap(), key);
    }

    #[test]
    fn a_cookie_round_trips_through_one_key() {
        let key = crypto::CookieKey::new([3u8; 32]).unwrap();
        let blob = key.encrypt("alive", ".example.com").unwrap();
        assert_eq!(&blob[..3], b"v10", "the wrapper both engines write");
        assert_eq!(
            blob.len(),
            3 + 12 + 32 + 5 + 16,
            "prefix + nonce + SHA-256(host) + value + tag"
        );
        assert_eq!(key.decrypt(&blob, ".example.com").unwrap(), "alive");
    }

    #[test]
    fn another_key_cannot_open_the_blob() {
        let source = crypto::CookieKey::new([1u8; 32]).unwrap();
        let other = crypto::CookieKey::new([2u8; 32]).unwrap();
        let blob = source.encrypt("alive", ".example.com").unwrap();
        assert!(other.decrypt(&blob, ".example.com").is_err());
    }

    #[test]
    fn a_blob_is_bound_to_the_host_it_was_written_for() {
        // The plaintext starts with SHA-256(host_key), so moving a blob to
        // another host's row is not a silent substitution - it fails.
        let key = crypto::CookieKey::new([4u8; 32]).unwrap();
        let blob = key.encrypt("alive", ".example.com").unwrap();
        assert!(key.decrypt(&blob, ".other.example").is_err());
    }

    #[test]
    fn a_full_copy_moves_every_readable_row() {
        let dir = scratch("full");
        let source_jar = Jar {
            cookies: dir.join("source").join("Cookies"),
            local_state: dir.join("source").join("Local State"),
            site_root: None,
        };
        let target_jar = Jar {
            cookies: dir.join("target").join("Cookies"),
            local_state: dir.join("target").join("Local State"),
            site_root: None,
        };

        let source_key = [11u8; 32];
        write_key(&source_jar.local_state, &source_key).unwrap();
        let source = jar_at(&source_jar.cookies, false);
        let cipher = crypto::CookieKey::new(source_key).unwrap();
        put(&source, ".example.com", "probe", cipher.encrypt("alive", ".example.com").unwrap());
        put(&source, "example.com", "second", cipher.encrypt("two", "example.com").unwrap());
        // A row this key cannot open, as a different Windows user's cookie would
        // be: it must be counted, not copied blind.
        put(&source, ".example.com", "foreign", vec![0x76, 0x31, 0x30, 1, 2, 3]);
        drop(source);

        let (copied, unreadable) = copy_app(&source_jar, &target_jar, true).unwrap();
        assert_eq!((copied, unreadable), (2, 1));

        // The target got a key of its own, and it is not the source's.
        let target_key = read_key(&target_jar.local_state).unwrap().unwrap();
        assert_ne!(target_key, source_key);
        let target = open(&target_jar.cookies).unwrap();
        let cipher = crypto::CookieKey::new(target_key).unwrap();
        assert_eq!(cipher.decrypt(&blob_of(&target, "probe"), ".example.com").unwrap(), "alive");
        assert_eq!(cipher.decrypt(&blob_of(&target, "second"), "example.com").unwrap(), "two");
        // The unreadable row is absent rather than present-and-broken.
        let missing: i64 = target
            .query_row("SELECT count(*) FROM cookies WHERE name = 'foreign'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(missing, 0);
        // And the jar the copy produced has the columns WebView2 expects.
        let columns: Vec<String> = target
            .prepare("PRAGMA table_info(cookies)")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        assert!(columns.iter().any(|c| c == "is_edgelegacycookie"));
        assert!(columns.iter().any(|c| c == "browser_provenance"));
        // And the `meta` rows it checks before adopting a jar at all: without
        // them WebView2 replaced the whole file with an empty one (measured).
        let keys: Vec<String> = target
            .prepare("SELECT key FROM meta ORDER BY key")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        assert!(keys.iter().any(|k| k == "edge_version_number"), "{keys:?}");
        assert!(
            keys.iter().any(|k| k == "compatible_edge_version_number"),
            "{keys:?}"
        );
    }

    #[test]
    fn a_target_that_already_has_cookies_keeps_the_ones_the_source_lacks() {
        let dir = scratch("merge");
        let source_jar = Jar {
            cookies: dir.join("source").join("Cookies"),
            local_state: dir.join("source").join("Local State"),
            site_root: None,
        };
        let target_jar = Jar {
            cookies: dir.join("target").join("Cookies"),
            local_state: dir.join("target").join("Local State"),
            site_root: None,
        };

        write_key(&source_jar.local_state, &[21u8; 32]).unwrap();
        let source = jar_at(&source_jar.cookies, false);
        let source_cipher = crypto::CookieKey::new([21u8; 32]).unwrap();
        put(&source, ".example.com", "shared", source_cipher.encrypt("new", ".example.com").unwrap());
        drop(source);

        // The target ran before: it has its own key and one cookie of its own.
        write_key(&target_jar.local_state, &[22u8; 32]).unwrap();
        let target = jar_at(&target_jar.cookies, false);
        let target_cipher = crypto::CookieKey::new([22u8; 32]).unwrap();
        put(&target, ".example.com", "kept", target_cipher.encrypt("mine", ".example.com").unwrap());
        drop(target);

        let (copied, unreadable) = copy_app(&source_jar, &target_jar, false).unwrap();
        assert_eq!((copied, unreadable), (1, 0));

        let target = open(&target_jar.cookies).unwrap();
        // The key did not change, so the target's own cookie still opens.
        assert_eq!(read_key(&target_jar.local_state).unwrap().unwrap(), [22u8; 32]);
        assert_eq!(target_cipher.decrypt(&blob_of(&target, "kept"), ".example.com").unwrap(), "mine");
        assert_eq!(target_cipher.decrypt(&blob_of(&target, "shared"), ".example.com").unwrap(), "new");
    }

    #[test]
    fn a_source_without_a_key_is_skipped_rather_than_copied_blind() {
        let dir = scratch("no-key");
        let source_jar = Jar {
            cookies: dir.join("source").join("Cookies"),
            local_state: dir.join("source").join("Local State"),
            site_root: None,
        };
        jar_at(&source_jar.cookies, false);
        let target_jar = Jar {
            cookies: dir.join("target").join("Cookies"),
            local_state: dir.join("target").join("Local State"),
            site_root: None,
        };
        let err = copy_app(&source_jar, &target_jar, true).unwrap_err();
        assert!(err.contains("no cookie key"), "{err}");
        assert!(!target_jar.cookies.exists(), "nothing was written");
    }

    #[test]
    fn writing_a_key_preserves_everything_else_in_local_state() {
        let dir = scratch("state");
        let state = dir.join("Local State");
        std::fs::write(&state, r#"{"os_crypt":{"audit_enabled":true},"other":{"keep":1}}"#).unwrap();
        write_key(&state, &[5u8; 32]).unwrap();
        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&state).unwrap()).unwrap();
        assert_eq!(json["other"]["keep"], 1);
        assert_eq!(json["os_crypt"]["audit_enabled"], true);
        assert!(json["os_crypt"]["encrypted_key"].is_string());
        assert_eq!(read_key(&state).unwrap(), Some([5u8; 32]));
    }

    #[test]
    fn a_missing_local_state_has_no_key_and_is_not_an_error() {
        let dir = scratch("absent");
        assert_eq!(read_key(&dir.join("Local State")).unwrap(), None);
    }

    /// A jar whose cookie files are irrelevant to a storage test, plus the
    /// profile root the stores hang off.
    fn storage_jar(root: &Path) -> Jar {
        Jar {
            cookies: root.join("Network").join("Cookies"),
            local_state: root.join("Local State"),
            site_root: Some(root.to_path_buf()),
        }
    }

    /// One file of `bytes` bytes at `path`, parents created.
    fn store_file(path: &Path, bytes: usize) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, vec![7u8; bytes]).unwrap();
    }

    /// The store a real app writes: a leveldb directory with one large table.
    fn whatsapp_store(profile: &Path, bytes: usize) -> PathBuf {
        let dir = profile
            .join("IndexedDB")
            .join("https_web.whatsapp.com_0.indexeddb.leveldb");
        store_file(&dir.join("000003.log"), bytes);
        dir
    }

    /// The case the feature exists for: the new engine has nothing, so the old
    /// engine's store is copied whole.
    #[test]
    fn an_empty_target_receives_the_sites_storage() {
        let dir = scratch("store-absent");
        let source = storage_jar(&dir.join("source"));
        let target = storage_jar(&dir.join("target"));
        std::fs::create_dir_all(dir.join("target")).unwrap();
        whatsapp_store(&dir.join("source"), 200 * 1024);

        match carry_site_storage(&source, &target).unwrap() {
            Carried::Copied { dirs, bytes } => {
                assert_eq!(dirs, 1, "one store was carried");
                assert_eq!(bytes, 200 * 1024, "the size reported is the copy's");
            }
            Carried::TargetInUse => panic!("an empty target cannot be in use"),
            Carried::Nothing => panic!("the source had a store to carry"),
        }
        let copied = dir
            .join("target")
            .join("IndexedDB")
            .join("https_web.whatsapp.com_0.indexeddb.leveldb")
            .join("000003.log");
        assert_eq!(std::fs::read(&copied).unwrap().len(), 200 * 1024);
    }

    /// Chromium creates the directories before the site writes anything, and a
    /// site that found no session writes a small database of its own. That stub
    /// is what made the user see a QR code, so it is replaced rather than
    /// protected.
    #[test]
    fn a_stub_target_is_replaced_instead_of_protected() {
        let dir = scratch("store-stub");
        let source = storage_jar(&dir.join("source"));
        let target = storage_jar(&dir.join("target"));
        whatsapp_store(&dir.join("source"), 200 * 1024);
        let stub = whatsapp_store(&dir.join("target"), 4096);
        store_file(&stub.join("stale.ldb"), 1024);

        match carry_site_storage(&source, &target).unwrap() {
            Carried::Copied { dirs, .. } => assert_eq!(dirs, 1),
            _ => panic!("a stub must not count as a session"),
        }
        assert_eq!(
            std::fs::read(stub.join("000003.log")).unwrap().len(),
            200 * 1024,
            "the stub's file was replaced by the real store"
        );
        assert!(
            !stub.join("stale.ldb").exists(),
            "the stub is replaced, not merged: two leveldbs cannot be merged by \
             copying files"
        );
    }

    /// A target of comparable size is a session the user has been using on the
    /// new engine. Overwriting it would sign them out to fix a problem they may
    /// no longer have.
    #[test]
    fn a_target_of_comparable_size_is_left_alone() {
        let dir = scratch("store-in-use");
        let source = storage_jar(&dir.join("source"));
        let target = storage_jar(&dir.join("target"));
        whatsapp_store(&dir.join("source"), 200 * 1024);
        let in_use = whatsapp_store(&dir.join("target"), 180 * 1024);
        let before = std::fs::read(in_use.join("000003.log")).unwrap();

        assert!(matches!(
            carry_site_storage(&source, &target).unwrap(),
            Carried::TargetInUse
        ));
        assert_eq!(
            std::fs::read(in_use.join("000003.log")).unwrap(),
            before,
            "the target's own storage was not touched"
        );
    }

    /// A profile that holds nothing but scaffolding is not a session, so the
    /// copy still runs. This is the state CEF leaves behind after one launch.
    #[test]
    fn scaffolding_alone_does_not_block_the_copy() {
        let dir = scratch("store-scaffold");
        let source = storage_jar(&dir.join("source"));
        let target = storage_jar(&dir.join("target"));
        whatsapp_store(&dir.join("source"), 200 * 1024);
        let scaffold = dir
            .join("target")
            .join("IndexedDB")
            .join("https_web.whatsapp.com_0.indexeddb.leveldb");
        std::fs::create_dir_all(&scaffold).unwrap();
        for name in ["CURRENT", "LOCK", "MANIFEST-000001"] {
            std::fs::write(scaffold.join(name), b"x").unwrap();
        }

        assert!(matches!(
            carry_site_storage(&source, &target).unwrap(),
            Carried::Copied { dirs: 1, .. }
        ));
    }

    /// The leveldb lives one directory down and the blob store beside it, so the
    /// copy has to keep the shape, not flatten it.
    #[test]
    fn nested_store_directories_keep_their_shape() {
        let dir = scratch("store-nested");
        let source = storage_jar(&dir.join("source"));
        let target = storage_jar(&dir.join("target"));
        let leveldb = dir
            .join("source")
            .join("IndexedDB")
            .join("https_site_0.indexeddb.leveldb");
        store_file(&leveldb.join("000005.ldb"), 64 * 1024);
        store_file(&leveldb.join("nested").join("deep.ldb"), 64 * 1024);
        store_file(
            &dir.join("source")
                .join("IndexedDB")
                .join("https_site_0.indexeddb.blob")
                .join("1")
                .join("0"),
            64 * 1024,
        );

        assert!(matches!(
            carry_site_storage(&source, &target).unwrap(),
            Carried::Copied { dirs: 1, .. }
        ));
        let copied = dir.join("target").join("IndexedDB");
        assert_eq!(
            std::fs::read(copied.join("https_site_0.indexeddb.leveldb").join("000005.ldb"))
                .unwrap()
                .len(),
            64 * 1024
        );
        assert_eq!(
            std::fs::read(
                copied
                    .join("https_site_0.indexeddb.leveldb")
                    .join("nested")
                    .join("deep.ldb")
            )
            .unwrap()
            .len(),
            64 * 1024
        );
        assert_eq!(
            std::fs::read(
                copied
                    .join("https_site_0.indexeddb.blob")
                    .join("1")
                    .join("0")
            )
            .unwrap()
            .len(),
            64 * 1024
        );
    }

    /// Nothing to carry is not a failure, and it must not report a copy.
    #[test]
    fn a_source_without_stores_carries_nothing() {
        let dir = scratch("store-empty");
        let source = storage_jar(&dir.join("source"));
        let target = storage_jar(&dir.join("target"));
        std::fs::create_dir_all(dir.join("source")).unwrap();
        std::fs::create_dir_all(dir.join("target")).unwrap();
        assert!(matches!(
            carry_site_storage(&source, &target).unwrap(),
            Carried::Nothing
        ));
    }

    /// A jar whose site root is unknown is skipped rather than guessed at: a
    /// wrong path would copy someone else's profile.
    #[test]
    fn an_unknown_site_root_is_not_guessed() {
        let dir = scratch("store-unknown");
        let mut source = storage_jar(&dir.join("source"));
        source.site_root = None;
        let target = storage_jar(&dir.join("target"));
        whatsapp_store(&dir.join("source"), 200 * 1024);
        assert!(matches!(
            carry_site_storage(&source, &target).unwrap(),
            Carried::Nothing
        ));
    }

    /// The boundary the "is this a session or a stub" decision rests on.
    #[test]
    fn worth_keeping_draws_the_line_at_a_quarter_of_the_source() {
        let min = MIN_STORE_BYTES;
        // Scaffolding only, whatever the source looks like.
        assert!(!worth_keeping(0, 1_000_000));
        assert!(!worth_keeping(min, 1_000_000), "at the floor is still empty");
        // Exactly a quarter is a session; a byte less is a stub.
        assert!(worth_keeping(min + 1, 4 * (min + 1)));
        assert!(!worth_keeping(min + 1, 4 * (min + 1) + 1));
        // The measured pair: 449 KB in a 386 MB source is a stub.
        assert!(!worth_keeping(449 * 1024, 386 * 1024 * 1024));
    }
}
