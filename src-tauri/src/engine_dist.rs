//! The optional Chromium engine: manifest, download, verify, install.
//!
//! RustBox ships one executable built against the OS webview. The Chromium build
//! is a **second executable** that cannot exist without its CEF payload, because
//! `libcef.dll` sits in that binary's PE import table: the Windows loader
//! resolves it before `main()` runs, so a CEF binary cannot start, report an
//! error, or fall back when the DLL is absent (ADR 0005 section 23.1). That is
//! why this module installs a whole engine directory rather than one library.
//!
//! # Trust
//!
//! The manifest is compiled into the executable with `include_str!`, never
//! fetched. A hash delivered over the same channel as the payload proves nothing
//! about the payload; a hash inside a signed binary is a trust anchor. The
//! archive is checked against it before a single file is unpacked.
//!
//! Authenticode is deliberately not checked: `libcef.dll` is unsigned in both
//! the official distribution and the codec-complete one (measured, ADR 0005
//! section 24.5), so checking it would reject every build that exists.
//!
//! # Layout
//!
//! ```text
//! rustbox\
//!   engines\cef\
//!     <version>\
//!       .ready            written last, only after a full verify
//!       libcef.dll, *.pak, icudtl.dat, locales\, ...
//!     <version>.partial\  never usable; deleted on the next launch
//! ```
//!
//! A `.partial` directory can never look like an installed engine, so an
//! interrupted download degrades to "not installed" instead of a broken launch.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Files a payload must contain before it counts as runnable.
const REQUIRED_FILES: &[&str] = &["libcef.dll", "resources.pak", "icudtl.dat"];

/// Directory name for an engine that finished unpacking.
///
/// One of two conditions; the other is the engine binary (see
/// [`is_installed_in`]). Kept as a separate marker rather than folded into a
/// single "installed" file because the two are written by different steps, and
/// a leftover payload from a failed staging has to be distinguishable from a
/// directory that was never populated.
const READY_MARKER: &str = ".ready";

/// Suffix for an install in progress.
const PARTIAL_SUFFIX: &str = ".partial";

/// One downloadable engine payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Payload {
    /// Engine version, e.g. `154.0.34`. This is the install directory name.
    pub version: String,
    /// Full CEF version string, for display and logs.
    pub cef_version: String,
    /// Chromium version the payload carries, for display.
    pub chromium_version: String,
    /// Release directory the archive comes from.
    pub source: String,
    /// Archive file name.
    pub file: String,
    /// Lowercase hex SHA-256 of the archive.
    pub sha256: String,
    /// Archive size in bytes, checked before hashing.
    pub size: u64,
    /// Platform this payload runs on: `windows-x86_64`, `linux-x86_64`,
    /// `macos-aarch64`, ...
    pub platform: String,
    /// Media codecs the build carries, e.g. `h264,aac`.
    #[serde(default)]
    pub codecs: String,
}

impl Payload {
    /// Where the archive is fetched from.
    pub fn url(&self) -> String {
        format!("{}/{}", self.source.trim_end_matches('/'), self.file)
    }
}

/// The compiled-in manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub schema: u32,
    #[serde(default)]
    pub note: String,
    pub entries: Vec<Payload>,
}

impl Manifest {
    /// The payload for the running platform, if the manifest has one.
    pub fn for_platform(&self, platform: &str) -> Option<&Payload> {
        self.entries.iter().find(|p| p.platform == platform)
    }
}

/// Parse the manifest compiled into this binary.
///
/// Panics on malformed JSON at first use, which is a build error rather than a
/// runtime one: the file is a source file, and a broken one should fail loudly
/// the first time anything asks for an engine instead of silently reporting
/// "nothing available".
pub fn manifest() -> &'static Manifest {
    use std::sync::OnceLock;
    static PARSED: OnceLock<Manifest> = OnceLock::new();
    PARSED.get_or_init(|| {
        serde_json::from_str(include_str!("../engine-manifest.json"))
            .expect("engine-manifest.json is not valid JSON")
    })
}

/// The `platform` key this build looks for in the manifest.
pub fn platform_key() -> &'static str {
    #[cfg(all(windows, target_arch = "x86_64"))]
    {
        "windows-x86_64"
    }
    #[cfg(all(windows, target_arch = "aarch64"))]
    {
        "windows-aarch64"
    }
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        "macos-aarch64"
    }
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    {
        "macos-x86_64"
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        "linux-x86_64"
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    {
        "linux-aarch64"
    }
    #[cfg(not(any(
        all(windows, any(target_arch = "x86_64", target_arch = "aarch64")),
        all(target_os = "macos", any(target_arch = "aarch64", target_arch = "x86_64")),
        all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64"))
    )))]
    {
        "unsupported"
    }
}

/// The payload this build would install, if one is published for the platform.
pub fn available() -> Option<&'static Payload> {
    manifest().for_platform(platform_key())
}

/// Root of the engine tree.
pub fn engines_root() -> PathBuf {
    crate::paths::root().join("engines").join("cef")
}

/// Directory an installed engine lives in.
pub fn version_dir(version: &str) -> PathBuf {
    version_dir_in(&engines_root(), version)
}

fn version_dir_in(root: &Path, version: &str) -> PathBuf {
    root.join(sanitize_version(version))
}

/// Directory an in-progress install writes to.
pub fn partial_dir(version: &str) -> PathBuf {
    partial_dir_in(&engines_root(), version)
}

fn partial_dir_in(root: &Path, version: &str) -> PathBuf {
    root.join(format!("{}{PARTIAL_SUFFIX}", sanitize_version(version)))
}

/// Versions are dotted numbers in practice, but they come from a file and end up
/// in a path, so anything that could escape the directory is dropped.
fn sanitize_version(version: &str) -> String {
    version
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '-' || *c == '_')
        .collect()
}

/// Whether `version` is installed *and* has its engine binary.
///
/// The binary is part of the definition of "installed", not a separate concern.
/// It used to be separate - `.ready` was written when the payload unpacked and
/// `stage_binary` ran afterwards - so a staging failure left a version that
/// `installed_versions()` reported as ready, that the UI offered a switch to,
/// and that could not start. Nothing repaired it either, because the only
/// startup repair path treats "no binary beside the launcher" as normal.
///
/// An in-progress `.partial` install is never installed, however complete its
/// files look: the name is what says the install never committed, and that
/// directory is deleted on the next attempt.
fn is_installed_in(root: &Path, version: &str) -> bool {
    !version.ends_with(PARTIAL_SUFFIX)
        && payload_complete_in(root, version)
        && version_dir_in(root, version).join(CEF_BINARY_NAME).is_file()
}

/// Whether the downloaded payload is unpacked and verified.
///
/// Deliberately weaker than [`is_installed_in`]: this is the state an install
/// reaches before the binary is staged, and it is what [`stage_binary_in`] has
/// to accept as a precondition rather than requiring its own postcondition.
///
/// This is also the re-entry guard in [`install_into`], so a retry after a
/// failed staging has to see it as true and finish the copy rather than
/// re-download 173 MB.
fn payload_complete_in(root: &Path, version: &str) -> bool {
    let dir = version_dir_in(root, version);
    dir.join(READY_MARKER).is_file() && REQUIRED_FILES.iter().all(|f| dir.join(f).is_file())
}

/// Every version that finished installing, newest name last.
///
/// Only runnable installs are listed: a directory whose payload unpacked but
/// whose binary was never staged is not an engine anyone can start, and
/// reporting it here is exactly what offered a switch to a dead engine. The
/// repair path uses [`unrunnable_versions`] instead.
pub fn installed_versions() -> Vec<String> {
    installed_versions_in(&engines_root())
}

/// Versions whose payload is intact but whose engine binary is missing.
///
/// These are repairable rather than broken: staging the binary beside the
/// launcher into the directory makes them runnable without a download. They are
/// deliberately absent from [`installed_versions`], so the UI does not offer a
/// switch to them until they are repaired.
pub fn unrunnable_versions() -> Vec<String> {
    unrunnable_versions_in(&engines_root())
}

fn installed_versions_in(root: &Path) -> Vec<String> {
    engine_dirs_in(root)
        .into_iter()
        .filter(|name| is_installed_in(root, name))
        .collect()
}

fn unrunnable_versions_in(root: &Path) -> Vec<String> {
    engine_dirs_in(root)
        .into_iter()
        .filter(|name| payload_complete_in(root, name) && !is_installed_in(root, name))
        .collect()
}

/// Every version directory, excluding in-progress `.partial` installs.
fn engine_dirs_in(root: &Path) -> Vec<String> {
    let mut out: Vec<String> = fs::read_dir(root)
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                .filter_map(|e| {
                    let name = e.file_name().to_string_lossy().into_owned();
                    (!name.ends_with(PARTIAL_SUFFIX)).then_some(name)
                })
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

/// Delete every `.partial` directory.
///
/// Called at startup. A crash or a killed download leaves one behind, and it is
/// never valid: the marker is written only after the archive verified and every
/// required file unpacked.
pub fn prune_partials() -> Vec<String> {
    let mut removed = Vec::new();
    let Ok(entries) = fs::read_dir(engines_root()) else {
        return removed;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.ends_with(PARTIAL_SUFFIX) {
            if fs::remove_dir_all(entry.path()).is_ok() {
                removed.push(name);
            }
        }
    }
    removed
}

/// Bytes an installed engine occupies.
pub fn disk_usage(version: &str) -> u64 {
    fn walk(dir: &Path) -> u64 {
        fs::read_dir(dir)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|e| match e.metadata() {
                        Ok(m) if m.is_dir() => walk(&e.path()),
                        Ok(m) => m.len(),
                        Err(_) => 0,
                    })
                    .sum()
            })
            .unwrap_or(0)
    }
    walk(&version_dir(version))
}

/// What the UI needs to describe the engine situation.
#[derive(Debug, Clone, Serialize)]
pub struct EngineStatus {
    /// Platform key this build looks for.
    pub platform: String,
    /// Whether the manifest has a payload for this platform.
    pub available: bool,
    /// Version the manifest would install.
    pub available_version: Option<String>,
    /// Chromium version that payload carries.
    pub chromium_version: Option<String>,
    /// Codecs the payload claims, e.g. `h264,aac`.
    pub codecs: Option<String>,
    /// Download size in bytes.
    pub download_size: Option<u64>,
    /// Versions already installed.
    pub installed: Vec<String>,
    /// Bytes each installed version occupies, keyed by version.
    pub installed_bytes: std::collections::HashMap<String, u64>,
}

/// Describe the engine situation without touching the network.
pub fn status() -> EngineStatus {
    let payload = available();
    let installed = installed_versions();
    let installed_bytes = installed
        .iter()
        .map(|v| (v.clone(), disk_usage(v)))
        .collect();
    EngineStatus {
        platform: platform_key().to_string(),
        available: payload.is_some(),
        available_version: payload.map(|p| p.version.clone()),
        chromium_version: payload.map(|p| p.chromium_version.clone()),
        codecs: payload.map(|p| p.codecs.clone()),
        download_size: payload.map(|p| p.size),
        installed,
        installed_bytes,
    }
}

/// Download, verify and install one payload, then stage this install's Chromium
/// build inside it.
///
/// `progress` is called with `(downloaded, total)`. The install is atomic from
/// the caller's point of view: either [`installed_versions`] reports the version
/// afterwards, or the call returns an error. Both halves are needed for that - a
/// payload without the binary is not an engine anyone can run, and reporting it as
/// installed is what left a broken version in place with no way to repair it.
pub fn install<F>(payload: &Payload, progress: F) -> Result<PathBuf, String>
where
    F: FnMut(u64, u64),
{
    let dir = install_into(&engines_root(), payload, progress)?;
    // A failure here leaves the payload on disk and the marker in place, which
    // `is_installed_in` still treats as *not* installed. The retry therefore
    // skips the download and repeats only the copy, instead of re-fetching
    // 173 MB.
    stage_binary(&payload.version)?;
    Ok(dir)
}

/// [`install`] against an explicit root, so the whole flow can be exercised in a
/// test against a local server instead of the user's data directory.
fn install_into<F>(root: &Path, payload: &Payload, mut progress: F) -> Result<PathBuf, String>
where
    F: FnMut(u64, u64),
{
    let target = version_dir_in(root, &payload.version);
    if payload_complete_in(root, &payload.version) {
        // Payload already verified and unpacked. Staging the binary is the
        // caller's job (`install`), and re-entering here after a failed staging
        // must not re-download 173 MB for a 33 MB copy.
        return Ok(target);
    }

    // A stale attempt must not be resumed: the archive is re-fetched and
    // re-hashed, because a partially written file cannot be verified.
    let partial = partial_dir_in(root, &payload.version);
    let _ = fs::remove_dir_all(&partial);
    fs::create_dir_all(&partial).map_err(|e| format!("cannot create {}: {e}", partial.display()))?;

    let archive = partial.join(&payload.file);
    let got = download(payload, &archive, &mut progress)?;
    if got != payload.size {
        let _ = fs::remove_dir_all(&partial);
        return Err(format!(
            "archive is {got} bytes, manifest says {}",
            payload.size
        ));
    }

    let actual = sha256_file(&archive)?;
    if !actual.eq_ignore_ascii_case(&payload.sha256) {
        let _ = fs::remove_dir_all(&partial);
        return Err(format!(
            "archive hash mismatch: got {actual}, expected {}",
            payload.sha256
        ));
    }

    let unpacked = partial.join("payload");
    fs::create_dir_all(&unpacked).map_err(|e| e.to_string())?;
    extract(&archive, &unpacked)?;

    for required in REQUIRED_FILES {
        if !unpacked.join(required).is_file() {
            let _ = fs::remove_dir_all(&partial);
            return Err(format!("payload is missing {required}"));
        }
    }

    // Commit. The directory only becomes visible under its real name once it is
    // complete, so a reader never sees a half-populated engine.
    if target.exists() {
        fs::remove_dir_all(&target).map_err(|e| e.to_string())?;
    }
    fs::rename(&unpacked, &target).map_err(|e| format!("cannot commit engine: {e}"))?;
    write_ready_marker(&target, payload)?;

    let _ = fs::remove_dir_all(&partial);
    Ok(target)
}

/// Write the marker that records a verified, fully unpacked payload.
///
/// This is not by itself "installed": [`is_installed_in`] also requires the
/// engine binary. The marker is written here, at the end of [`install_into`], and
/// the binary is staged afterwards by [`install`], so the two states are
/// distinguishable on disk. Writing a marker that meant "installed" before the
/// binary existed was a real bug: a staging failure then left a directory that
/// `installed_versions()` reported as ready, that the UI offered a switch to, and
/// that could not start - and nothing repaired it, because the only startup
/// repair path treated "no binary beside the launcher" as normal.
fn write_ready_marker(target: &Path, payload: &Payload) -> Result<(), String> {
    fs::write(
        target.join(READY_MARKER),
        format!(
            "{{\"version\":\"{}\",\"sha256\":\"{}\"}}",
            payload.version, payload.sha256
        ),
    )
    .map_err(|e| e.to_string())
}

/// Stream `payload` to `dest`, returning the byte count.
fn download<F>(payload: &Payload, dest: &Path, progress: &mut F) -> Result<u64, String>
where
    F: FnMut(u64, u64),
{
    let url = payload.url();
    let mut response = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(60 * 30))
        .build()
        .map_err(|e| e.to_string())?
        .get(&url)
        .send()
        .map_err(|e| format!("cannot fetch {url}: {e}"))?;

    if !response.status().is_success() {
        return Err(format!("{url} returned HTTP {}", response.status()));
    }

    let total = response.content_length().unwrap_or(payload.size);
    let mut file = fs::File::create(dest).map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; 256 * 1024];
    let mut got: u64 = 0;
    progress(0, total);
    loop {
        let n = response.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n]).map_err(|e| e.to_string())?;
        got += n as u64;
        progress(got, total);
    }
    file.flush().map_err(|e| e.to_string())?;
    Ok(got)
}

/// SHA-256 of a file, lowercase hex.
fn sha256_file(path: &Path) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    let mut file = fs::File::open(path).map_err(|e| e.to_string())?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = file.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// Unpack a CEF distribution into `dest`.
///
/// The official layout separates runtime files into `Release\` and
/// `Resources\`, which is a build tree, not a deployment one: CEF looks for
/// `libcef.dll`, the `.pak` files and `icudtl.dat` beside the executable. This
/// flattens both directories into `dest`, keeps the license files, and skips
/// everything only a CEF *build* needs (`include\`, `libcef_dll\`, `cmake\`,
/// `bazel\`, `libcef.lib`).
fn extract(archive: &Path, dest: &Path) -> Result<(), String> {
    let file = fs::File::open(archive).map_err(|e| e.to_string())?;
    let decoder = bzip2::read::BzDecoder::new(std::io::BufReader::new(file));
    let mut tar = tar::Archive::new(decoder);

    for entry in tar.entries().map_err(|e| e.to_string())? {
        let mut entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path().map_err(|e| e.to_string())?.into_owned();

        // Strip the archive's single top-level directory.
        let rel: PathBuf = path
            .components()
            .skip(1)
            .collect::<std::path::PathBuf>();
        if rel.as_os_str().is_empty() {
            continue;
        }
        let rel_str = rel.to_string_lossy().replace('\\', "/");

        let deploy = match rel_str.split_once('/') {
            // Runtime files: flattened.
            Some(("Release", rest)) => Some(rest.to_string()),
            // Resources: locales and the pak/dat files, flattened.
            Some(("Resources", rest)) => Some(rest.to_string()),
            // At the top level only the licence is worth shipping; CREDITS.html
            // alone is 8.5 MB of the archive.
            None if rel_str == "LICENSE.txt" => Some(rel_str.clone()),
            None => None,
            // Build-only trees.
            Some(("include", _)) | Some(("libcef_dll", _)) | Some(("cmake", _))
            | Some(("bazel", _)) => None,
            Some(_) => None,
        };
        let Some(rel_out) = deploy else { continue };

        // `libcef.lib` is an import library; the runtime never loads it.
        if rel_out == "libcef.lib" {
            continue;
        }
        // Locales are 48 MB of the 418 MB and only `en-US` is ever selected
        // unless the user changes the UI language.
        if let Some(locale) = rel_out.strip_prefix("locales/") {
            if !matches!(locale, "en-US.pak" | "id.pak") {
                continue;
            }
        }

        let out = dest.join(&rel_out);
        if let Some(parent) = out.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        if entry.header().entry_type().is_dir() {
            fs::create_dir_all(&out).map_err(|e| e.to_string())?;
        } else {
            entry.unpack(&out).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

/// Path to the Chromium build's executable, when the engine is installed.
///
/// The executable lives inside the engine directory because that is where
/// `libcef.dll` and the `.pak` files must be: CEF resolves its resources
/// relative to the process image, so the binary cannot be moved next to the
/// main app.
pub fn binary(version: &str) -> Option<PathBuf> {
    binary_in(&engines_root(), version)
}

fn binary_in(root: &Path, version: &str) -> Option<PathBuf> {
    if !is_installed_in(root, version) {
        return None;
    }
    let exe = version_dir_in(root, version).join(CEF_BINARY_NAME);
    exe.is_file().then_some(exe)
}

/// Name of the Chromium build's executable inside the engine directory.
#[cfg(windows)]
const CEF_BINARY_NAME: &str = "rustbox-cef.exe";
#[cfg(not(windows))]
const CEF_BINARY_NAME: &str = "rustbox-cef";

/// Copy this install's Chromium build into an installed engine directory.
///
/// The downloaded payload is the Chromium *runtime* - `libcef.dll`, the `.pak`
/// files, `icudtl.dat` - but not the app. `rustbox-cef.exe` is this project's
/// second build, and it has to live inside the engine directory because CEF
/// resolves its resources relative to the process image; next to the launcher it
/// would find no `libcef.dll` and no locale packs.
///
/// So the installer ships the Chromium build beside the launcher and this
/// function moves it in. Called after a successful install, and on every start
/// for an engine installed by an older build: the copy is skipped when the sizes
/// and timestamps already match, so the common case costs one metadata read.
pub fn stage_binary(version: &str) -> Result<PathBuf, String> {
    let source = cef_binary_beside_exe().ok_or_else(|| {
        "could not locate this install's Chromium build".to_string()
    })?;
    stage_binary_in(&engines_root(), &source, version)
}

/// [`stage_binary`], but silent when this install has no Chromium build.
///
/// Called on every start for whatever engine is installed. A launcher built from
/// source, or an install that ships only the launcher, legitimately has no
/// `rustbox-cef` beside it; that is not an error worth a log line on every start.
#[cfg(feature = "wry")]
pub fn stage_binary_if_present(version: &str) -> Result<Option<PathBuf>, String> {
    let Some(source) = cef_binary_beside_exe() else {
        return Ok(None);
    };
    stage_binary_in(&engines_root(), &source, version).map(Some)
}

fn cef_binary_beside_exe() -> Option<PathBuf> {
    let path = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(CEF_BINARY_NAME)))?;
    path.is_file().then_some(path)
}

fn stage_binary_in(root: &Path, source: &Path, version: &str) -> Result<PathBuf, String> {
    let dir = version_dir_in(root, version);
    // The payload must be there; the binary is what this function produces, so
    // requiring it here would be a precondition that is its own postcondition.
    if !payload_complete_in(root, version) {
        return Err(format!("engine {version} is not installed"));
    }
    let dest = dir.join(CEF_BINARY_NAME);
    // Running from the engine directory already (the Chromium build itself, or a
    // dev run of it): the source and destination are the same file.
    if source == dest {
        return Ok(dest);
    }
    if !source.is_file() {
        return Err(format!(
            "this install has no Chromium build at {}; reinstall RustBox",
            source.display()
        ));
    }

    let unchanged = match (fs::metadata(source), fs::metadata(&dest)) {
        (Ok(a), Ok(b)) => a.len() == b.len() && a.modified().ok() == b.modified().ok(),
        _ => false,
    };
    if !unchanged {
        // Copy beside the destination and rename, so a crash cannot leave a
        // half-written executable that the loader would happily try to run.
        let tmp = dest.with_extension("exe.tmp");
        fs::copy(source, &tmp).map_err(|e| format!("could not stage the engine binary: {e}"))?;
        fs::rename(&tmp, &dest).map_err(|e| format!("could not stage the engine binary: {e}"))?;
    }
    Ok(dest)
}

/// Remove an installed engine, and any in-progress install of the same version.
pub fn remove(version: &str) -> Result<(), String> {
    let dir = version_dir(version);
    if dir.exists() {
        fs::remove_dir_all(&dir).map_err(|e| format!("cannot remove {}: {e}", dir.display()))?;
    }
    // A cancelled download must go too, otherwise "remove" leaves the bytes the
    // user was trying to reclaim.
    let partial = partial_dir(version);
    if partial.exists() {
        fs::remove_dir_all(&partial).map_err(|e| format!("cannot remove {}: {e}", partial.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_parses_and_matches_its_schema() {
        let m = manifest();
        assert_eq!(m.schema, 1, "manifest schema changed; update this test");
        assert!(!m.entries.is_empty(), "manifest has no payloads");
    }

    #[test]
    fn every_payload_has_a_well_formed_hash_and_size() {
        for p in &manifest().entries {
            assert_eq!(p.sha256.len(), 64, "{}: sha256 is not hex-64", p.version);
            assert!(
                p.sha256.chars().all(|c| c.is_ascii_hexdigit()),
                "{}: sha256 has non-hex characters",
                p.version
            );
            assert!(p.size > 0, "{}: size is zero", p.version);
            assert!(!p.file.is_empty(), "{}: empty archive name", p.version);
            assert!(
                p.url().starts_with("https://"),
                "{}: payload must be fetched over https, got {}",
                p.version,
                p.url()
            );
        }
    }

    #[test]
    fn platform_key_is_known() {
        // A typo here would make the feature silently report "nothing available"
        // on the one platform it was built for.
        assert_ne!(platform_key(), "unsupported");
    }

    #[test]
    fn version_cannot_escape_the_engine_directory() {
        assert_eq!(sanitize_version("154.0.34"), "154.0.34");
        assert_eq!(sanitize_version("../../etc"), "....etc");
        assert_eq!(sanitize_version("a/b\\c"), "abc");
    }

    #[test]
    fn a_payload_only_install_is_reported_as_repairable_and_then_repairs() {
        // The whole lifecycle of the bug this contract exists for, against a temp
        // root so it never touches the user's engine directory:
        //
        //   payload unpacked, no binary  -> not installed, but listed as repairable
        //   binary staged                -> installed, no longer repairable
        //
        // The middle state is the one that was previously invisible: it reported
        // as installed (offering a switch to an engine that cannot start) and no
        // code path repaired it.
        let root = scratch("repair");
        let dir = version_dir_in(&root, "x");
        fs::create_dir_all(&dir).unwrap();
        for f in REQUIRED_FILES {
            fs::write(dir.join(f), b"x").unwrap();
        }
        fs::write(dir.join(READY_MARKER), b"{}").unwrap();

        assert_eq!(installed_versions_in(&root), Vec::<String>::new());
        assert_eq!(unrunnable_versions_in(&root), vec!["x".to_string()]);

        // Repair: the same copy the startup loop and the Repair button perform.
        let source = root.join("rustbox-cef-shipped.exe");
        fs::write(&source, b"exe").unwrap();
        stage_binary_in(&root, &source, "x").expect("staging a payload-only install must succeed");

        assert_eq!(installed_versions_in(&root), vec!["x".to_string()]);
        assert_eq!(
            unrunnable_versions_in(&root),
            Vec::<String>::new(),
            "a repaired install must stop being offered as repairable"
        );
        assert!(dir.join(CEF_BINARY_NAME).is_file());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_partial_install_is_neither_installed_nor_repairable() {
        // A `.partial` is an interrupted download: its files are incomplete and
        // cannot be verified, so it must not appear in either list. Repairing it
        // would stage a binary into a directory that is about to be deleted.
        let root = scratch("partial-lists");
        let dir = version_dir_in(&root, "y.partial");
        fs::create_dir_all(&dir).unwrap();
        for f in REQUIRED_FILES {
            fs::write(dir.join(f), b"x").unwrap();
        }
        fs::write(dir.join(READY_MARKER), b"{}").unwrap();

        assert_eq!(installed_versions_in(&root), Vec::<String>::new());
        assert_eq!(unrunnable_versions_in(&root), Vec::<String>::new());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn partial_directory_is_not_a_version_directory() {
        // `installed_versions` must never report a `.partial` as installed, and
        // `is_installed_in` must never accept one. Against a scratch root rather
        // than the user's engine directory: the old version of this test read the
        // real one, so its result depended on what happened to be installed on the
        // machine running it.
        let root = scratch("partial-name");
        let dir = version_dir_in(&root, "154.0.34.partial");
        fs::create_dir_all(&dir).unwrap();
        for f in REQUIRED_FILES {
            fs::write(dir.join(f), b"x").unwrap();
        }
        fs::write(dir.join(CEF_BINARY_NAME), b"exe").unwrap();
        fs::write(dir.join(READY_MARKER), b"{}").unwrap();

        // Even a fully populated `.partial` is not a version: the name is what
        // says the install never committed.
        assert!(!is_installed_in(&root, "154.0.34.partial"));
        assert!(
            installed_versions_in(&root).is_empty(),
            "a partial install leaked into the installed list"
        );
        assert!(
            engine_dirs_in(&root).is_empty(),
            "a partial install must not be listed as a version directory at all"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn payload_url_joins_without_doubling_slashes() {
        let p = Payload {
            version: "1".into(),
            cef_version: "1".into(),
            chromium_version: "1".into(),
            source: "https://example.test/releases/tag/".into(),
            file: "a.tar.bz2".into(),
            sha256: "0".repeat(64),
            size: 1,
            platform: "windows-x86_64".into(),
            codecs: String::new(),
        };
        assert_eq!(
            p.url(),
            "https://example.test/releases/tag/a.tar.bz2",
            "a trailing slash in `source` must not produce a double slash"
        );
    }

    /// Build a CEF-shaped archive: the layout the real distribution uses, plus
    /// the trees a deployment must drop.
    fn make_archive() -> Vec<u8> {
        let root = "cef_binary_154.0.34_test_windows64_minimal";
        let entries: Vec<(String, &[u8])> = vec![
            // Runtime, under Release\ -> flattened.
            (format!("{root}/Release/libcef.dll"), b"dll" as &[u8]),
            (format!("{root}/Release/chrome_elf.dll"), b"elf"),
            (format!("{root}/Release/libcef.lib"), b"import-lib"),
            // Runtime, under Resources\ -> flattened.
            (format!("{root}/Resources/resources.pak"), b"pak"),
            (format!("{root}/Resources/icudtl.dat"), b"dat"),
            (format!("{root}/Resources/locales/en-US.pak"), b"en"),
            (format!("{root}/Resources/locales/fr.pak"), b"fr"),
            // Build-only, must be dropped.
            (format!("{root}/include/cef_app.h"), b"header"),
            (format!("{root}/libcef_dll/wrapper.cc"), b"src"),
            (format!("{root}/cmake/cef.cmake"), b"cmake"),
            (format!("{root}/CREDITS.html"), b"credits"),
            (format!("{root}/LICENSE.txt"), b"bsd"),
        ];

        let mut out = Vec::new();
        {
            let enc = bzip2::write::BzEncoder::new(&mut out, bzip2::Compression::fast());
            let mut builder = tar::Builder::new(enc);
            for (name, data) in &entries {
                let mut header = tar::Header::new_gnu();
                header.set_size(data.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                builder.append_data(&mut header, name, *data).unwrap();
            }
            builder.into_inner().unwrap().finish().unwrap();
        }
        out
    }

    /// Serve `body` once on a loopback port and return its base URL.
    fn serve_once(body: Vec<u8>) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 2048];
                let _ = stream.read(&mut buf);
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&body);
                let _ = stream.flush();
            }
        });
        format!("http://{addr}")
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rustbox-engine-test-{}-{name}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn payload_for(url: String, body: &[u8], sha: String) -> Payload {
        Payload {
            version: "test-1".into(),
            cef_version: "test".into(),
            chromium_version: "test".into(),
            source: url,
            file: "payload.tar.bz2".into(),
            sha256: sha,
            size: body.len() as u64,
            platform: "test".into(),
            codecs: "h264,aac".into(),
        }
    }

    #[test]
    fn install_flattens_the_distribution_and_marks_it_ready() {
        let body = make_archive();
        let sha = {
            use sha2::{Digest, Sha256};
            Sha256::digest(&body)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        };
        let url = serve_once(body.clone());
        let payload = payload_for(url, &body, sha);
        let root = scratch("ok");

        let dir = install_into(&root, &payload, |_, _| {}).expect("install should succeed");

        // Flattened runtime files.
        for f in ["libcef.dll", "chrome_elf.dll", "resources.pak", "icudtl.dat"] {
            assert!(dir.join(f).is_file(), "{f} should be beside the executable");
        }
        assert!(dir.join("locales/en-US.pak").is_file());
        assert!(dir.join("LICENSE.txt").is_file(), "licence must ship");

        // Dropped: build trees, the import library, other locales, credits.
        assert!(!dir.join("libcef.lib").exists(), "import lib is link-time only");
        assert!(!dir.join("include").exists());
        assert!(!dir.join("libcef_dll").exists());
        assert!(!dir.join("cmake").exists());
        assert!(!dir.join("locales/fr.pak").exists(), "only shipped locales");
        assert!(!dir.join("CREDITS.html").exists());

        assert!(dir.join(".ready").is_file());
        // The payload is complete, but `install_into` deliberately does not stage
        // the binary (that is `install`'s last step), so this is not yet
        // "installed" in the runnable sense.
        assert!(payload_complete_in(&root, "test-1"));
        assert!(!is_installed_in(&root, "test-1"));
        assert!(
            !partial_dir_in(&root, "test-1").exists(),
            "the partial directory must be gone once the install commits"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_wrong_hash_installs_nothing() {
        let body = make_archive();
        let url = serve_once(body.clone());
        let payload = payload_for(url, &body, "0".repeat(64));
        let root = scratch("badhash");

        let err = install_into(&root, &payload, |_, _| {}).expect_err("hash must be checked");
        assert!(err.contains("hash mismatch"), "got: {err}");

        assert!(!is_installed_in(&root, "test-1"));
        assert!(
            !version_dir_in(&root, "test-1").exists(),
            "a rejected archive must not leave a version directory"
        );
        assert!(
            !partial_dir_in(&root, "test-1").exists(),
            "a rejected archive must not leave a partial directory"
        );

        let _ = fs::remove_dir_all(&root);
    }

    fn dir_bytes(dir: &Path) -> u64 {
        fs::read_dir(dir)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|e| match e.metadata() {
                        Ok(m) if m.is_dir() => dir_bytes(&e.path()),
                        Ok(m) => m.len(),
                        Err(_) => 0,
                    })
                    .sum()
            })
            .unwrap_or(0)
    }

    /// Real-archive check, run by hand:
    ///
    /// ```text
    /// set RUSTBOX_CEF_ARCHIVE=...\cef_binary_..._windows64_minimal.tar.bz2
    /// set RUSTBOX_CEF_SHA256=af1f1887...
    /// cargo test --bin rustbox real_payload -- --ignored
    /// ```
    ///
    /// The synthetic archive above pins the layout the code expects. This one
    /// proves the parser survives what the codec-complete build actually ships:
    /// PAX headers, real ordering, and 165.7 MB of it.
    #[test]
    #[ignore = "reads a 165 MB archive from RUSTBOX_CEF_ARCHIVE"]
    fn real_payload_installs() {
        let path = std::env::var("RUSTBOX_CEF_ARCHIVE").expect("RUSTBOX_CEF_ARCHIVE");
        let body = fs::read(&path).expect("archive");
        let sha = std::env::var("RUSTBOX_CEF_SHA256").expect("RUSTBOX_CEF_SHA256");
        let url = serve_once(body.clone());
        let mut payload = payload_for(url, &body, sha);
        payload.version = "real".into();
        let root = scratch("real");

        let dir = install_into(&root, &payload, |_, _| {}).expect("real payload should install");

        assert!(dir.join("libcef.dll").is_file());
        assert!(dir.join("resources.pak").is_file());
        assert!(dir.join("icudtl.dat").is_file());
        assert!(dir.join("chrome_elf.dll").is_file());
        assert!(dir.join("locales/en-US.pak").is_file());
        assert!(!dir.join("include").exists());
        assert!(!dir.join("libcef.lib").exists());
        assert!(payload_complete_in(&root, "real"));

        let total = dir_bytes(&dir);
        println!("installed payload: {} MB", total / (1024 * 1024));
        // The distribution is 418 MB with headers and 220 locales. A deployment
        // without them must be materially smaller - that is the point of
        // flattening rather than unzipping the whole build tree.
        assert!(total > 250 * 1024 * 1024, "payload looks truncated: {total}");
        assert!(total < 400 * 1024 * 1024, "payload kept build files: {total}");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_payload_without_its_binary_is_not_installed() {
        let root = scratch("binary");

        // Directory exists but is not installed: no binary claimed.
        fs::create_dir_all(version_dir_in(&root, "x")).unwrap();
        assert!(!is_installed_in(&root, "x"));
        assert!(binary_in(&root, "x").is_none());

        // Complete payload but no executable yet. This is the state a failed
        // staging leaves behind, and it must NOT count as installed: reporting
        // it as installed is what offered the user a switch to an engine that
        // could not start.
        let dir = version_dir_in(&root, "x");
        for f in REQUIRED_FILES {
            fs::write(dir.join(f), b"x").unwrap();
        }
        fs::write(dir.join(READY_MARKER), b"{}").unwrap();
        assert!(payload_complete_in(&root, "x"), "the payload itself is complete");
        assert!(
            !is_installed_in(&root, "x"),
            "an install without its executable is not runnable"
        );
        assert!(binary_in(&root, "x").is_none());

        // Executable present: now it is.
        fs::write(dir.join(CEF_BINARY_NAME), b"exe").unwrap();
        assert!(is_installed_in(&root, "x"));
        let exe = binary_in(&root, "x").expect("a complete install offers its executable");
        assert_eq!(exe, dir.join(CEF_BINARY_NAME));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_complete_payload_is_not_downloaded_again() {
        // The retry path after a failed staging. `install_into` has to treat an
        // unpacked payload as done even though `is_installed_in` is still false,
        // or recovering from a 33 MB copy failure costs another 173 MB download.
        let root = scratch("reentry");
        let dir = version_dir_in(&root, "x");
        fs::create_dir_all(&dir).unwrap();
        for f in REQUIRED_FILES {
            fs::write(dir.join(f), b"x").unwrap();
        }
        fs::write(dir.join(READY_MARKER), b"{}").unwrap();

        // Any URL will do: reaching the network at all is the failure.
        let payload = payload_for("http://127.0.0.1:1/never".into(), b"x", "0".repeat(64));
        let mut version = payload.clone();
        version.version = "x".into();
        let got = install_into(&root, &version, |_, _| {})
            .expect("an unpacked payload must not be re-fetched");
        assert_eq!(got, dir);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_truncated_archive_installs_nothing() {
        let body = make_archive();
        let sha = {
            use sha2::{Digest, Sha256};
            Sha256::digest(&body)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        };
        // Advertise the full size but serve a truncated body: the size check
        // must reject it before the hash is even reached.
        let truncated = body[..body.len() / 2].to_vec();
        let url = serve_once(truncated);
        let payload = payload_for(url, &body, sha);
        let root = scratch("truncated");

        let err = install_into(&root, &payload, |_, _| {}).expect_err("size must be checked");
        assert!(err.contains("bytes"), "got: {err}");
        assert!(!is_installed_in(&root, "test-1"));
        assert!(!partial_dir_in(&root, "test-1").exists());

        let _ = fs::remove_dir_all(&root);
    }
}
