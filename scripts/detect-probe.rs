// Standalone harness: copies `anti_detect_browsers` + helpers verbatim from
// src-tauri/src/external.rs to show exactly what `detect_browsers` yields on
// this machine. Not part of the build.
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
struct DetectedBrowser {
    name: String,
    path: String,
    family: String,
    source: String,
}

fn family_label(path: &str) -> &'static str {
    let name = Path::new(path)
        .file_stem()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if name.contains("camoufox") {
        "camoufox"
    } else if name.contains("firefox")
        || name.contains("librewolf")
        || name.contains("waterfox")
        || name.contains("tor browser")
        || name.contains("torbrowser")
    {
        "firefox"
    } else {
        "chromium"
    }
}

fn source_label(path: &Path) -> String {
    let parts: Vec<String> = path
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();
    let at = |i: usize| parts.get(i).cloned().unwrap_or_default();

    for (i, part) in parts.iter().enumerate() {
        let lower = part.to_lowercase();
        if lower == "local" && i > 0 && parts[i - 1].eq_ignore_ascii_case("appdata") {
            return at(i + 1);
        }
        if lower == "program files" || lower == "program files (x86)" {
            return at(i + 1);
        }
        if lower == "users" {
            let owner = at(i + 2);
            if owner.starts_with('.') {
                return owner;
            }
        }
    }
    parts.len().checked_sub(2).map(at).unwrap_or_default()
}

fn push_if_exists(out: &mut Vec<DetectedBrowser>, name: &str, path: PathBuf) {
    if path.is_file() {
        let path_str = path.to_string_lossy().into_owned();
        if !out.iter().any(|b| b.path == path_str) {
            let source = source_label(&path);
            out.push(DetectedBrowser {
                name: name.to_string(),
                path: path_str.clone(),
                family: family_label(&path_str).to_string(),
                source,
            });
        }
    }
}

fn is_executable_named(path: &Path, stem: &str) -> bool {
    path.file_name()
        .map(|f| f.to_string_lossy().to_lowercase())
        .and_then(|f| f.strip_suffix(".exe").map(str::to_string))
        .map(|f| f == stem)
        .unwrap_or(false)
}

fn shallow_exes(root: &Path, stem: &str, depth: usize) -> Vec<PathBuf> {
    fn walk(dir: &Path, stem: &str, depth: usize, out: &mut Vec<PathBuf>) {
        if depth == 0 || out.len() >= 4 {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_dir() {
                walk(&path, stem, depth - 1, out);
            } else if is_executable_named(&path, stem) {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, stem, depth, &mut out);
    out
}

fn anti_detect_browsers() -> Vec<DetectedBrowser> {
    let mut out = Vec::new();
    let local = std::env::var("LOCALAPPDATA").ok().map(PathBuf::from);
    let home = std::env::var("USERPROFILE").ok().map(PathBuf::from);

    if let Some(local) = &local {
        let live = local.join(r"camoufox\camoufox\Cache\camoufox.exe");
        if live.is_file() {
            push_if_exists(&mut out, "camoufox", live);
        } else {
            push_if_exists(
                &mut out,
                "camoufox",
                local.join(r"camoufox\camoufox\Cache.stock.bak\camoufox.exe"),
            );
        }
        for rel in [r"DonutBrowser\binaries\camoufox", r"DonutBrowser\binaries"] {
            for exe in shallow_exes(&local.join(rel), "camoufox", 3) {
                push_if_exists(&mut out, "camoufox", exe);
            }
        }
        for rel in [r"cloakbrowser", r"CloakBrowser"] {
            for exe in shallow_exes(&local.join(rel), "chrome", 3) {
                push_if_exists(&mut out, "cloakbrowser", exe);
            }
        }
    }
    if let Some(home) = &home {
        for exe in shallow_exes(&home.join(".cloakbrowser"), "chrome", 3) {
            push_if_exists(&mut out, "cloakbrowser", exe);
        }
    }
    out
}

fn main() {
    let found = anti_detect_browsers();
    println!("count = {}", found.len());
    for b in &found {
        println!("  {:<12} {:<9} {:<14} {}", b.name, b.family, b.source, b.path);
    }
}
