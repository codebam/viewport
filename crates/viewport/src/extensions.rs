// SPDX-License-Identifier: GPL-3.0-or-later
//
// User shell extensions: directory discovery, manifest validation, strict
// path containment, and content-fingerprinted revisions for live hot-reload.
//
// Each extension lives in its own subdirectory `<dir>/<id>/` containing an
// `extension.json` manifest plus local JavaScript (and optional CSS) assets.
// The compositor discovers and validates them, resolves every asset path to a
// canonical `file://` URL strictly contained inside the extension's directory,
// and pushes `extensions.list` / `extensions.changed` events to the shell.

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;
use viewport_ipc::event::{ExtensionSlot, ShellExtension};

/// Maximum length of an extension identifier.
const MAX_ID_LEN: usize = 64;

/// Maximum manifest size accepted from disk (256 KiB).
const MAX_MANIFEST_BYTES: u64 = 256 * 1024;

/// Maximum asset file size hashed for content fingerprinting (4 MiB).
/// Larger assets fall back to `(len, mtime)` metadata fingerprinting so a
/// huge file cannot stall the compositor loop.
const MAX_HASH_BYTES: u64 = 4 * 1024 * 1024;

/// Policy controlling which discovered extensions are active.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    /// Master switch. False disables all user shell extensions.
    pub enabled: bool,
    /// IDs explicitly enabled (overrides `"enabled": false` in `extension.json`).
    pub enable: Vec<String>,
    /// IDs explicitly disabled (wins over both `enable` and manifest `enabled`).
    pub disable: Vec<String>,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            enabled: true,
            enable: Vec::new(),
            disable: Vec::new(),
        }
    }
}

/// On-disk `extension.json` manifest inside `<root>/<id>/`.
#[derive(Debug, Deserialize)]
struct Manifest {
    id: String,
    #[serde(default)]
    name: Option<String>,
    slot: ExtensionSlot,
    script: String,
    #[serde(default)]
    style: Option<String>,
    #[serde(default = "default_enabled")]
    enabled: bool,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    options: Option<serde_json::Value>,
}

fn default_enabled() -> bool {
    true
}

/// Default user extension root: `$XDG_CONFIG_HOME/viewport/extensions` or
/// `$HOME/.config/viewport/extensions`.
pub fn default_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME") {
        if !dir.is_empty() {
            return Some(PathBuf::from(dir).join("viewport/extensions"));
        }
    }
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".config/viewport/extensions"))
}

/// Default extension root directories as a `Vec` (empty only if neither
/// `XDG_CONFIG_HOME` nor `HOME` is set).
pub fn default_dirs() -> Vec<PathBuf> {
    default_dir().into_iter().collect()
}

/// Whether `id` is a safe, canonical extension identifier:
/// 1..=64 ASCII chars, starting with `[A-Za-z0-9]`, containing only
/// `[A-Za-z0-9_-]`. Never allows `.`, `..`, path separators, or whitespace.
pub fn valid_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    if bytes.is_empty() || bytes.len() > MAX_ID_LEN {
        return false;
    }
    bytes[0].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

/// Resolve an extension-relative asset path and strictly verify containment
/// inside `canonical_ext_dir`.
///
/// Rejects:
/// - empty paths, NUL bytes, and URL schemes (`://`),
/// - absolute paths and any `..` parent traversal components,
/// - symlinks or paths that canonicalize outside `canonical_ext_dir`,
/// - missing files or non-regular files (e.g. directories).
pub fn verify_asset_path(canonical_ext_dir: &Path, relative: &str) -> anyhow::Result<PathBuf> {
    let trimmed = relative.trim();
    anyhow::ensure!(!trimmed.is_empty(), "asset path is empty");
    anyhow::ensure!(!trimmed.contains('\0'), "asset path contains NUL byte");
    anyhow::ensure!(
        !trimmed.contains("://"),
        "remote or scheme URLs are not allowed in extension manifests"
    );

    let rel_path = Path::new(trimmed);
    anyhow::ensure!(
        !rel_path.is_absolute(),
        "extension asset path must be relative to the extension directory"
    );
    for component in rel_path.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                anyhow::bail!("path traversal is not allowed in extension asset path {trimmed:?}");
            }
        }
    }

    let candidate = canonical_ext_dir.join(rel_path);
    let canonical = std::fs::canonicalize(&candidate)
        .map_err(|e| anyhow::anyhow!("cannot resolve asset {trimmed:?}: {e}"))?;
    anyhow::ensure!(
        canonical.starts_with(canonical_ext_dir),
        "asset {trimmed:?} escapes extension directory {}",
        canonical_ext_dir.display()
    );
    let meta = std::fs::metadata(&canonical)
        .map_err(|e| anyhow::anyhow!("cannot stat asset {trimmed:?}: {e}"))?;
    anyhow::ensure!(meta.is_file(), "asset {trimmed:?} is not a regular file");
    Ok(canonical)
}

/// Build a `file://` URL with a cache-busting revision query parameter.
pub fn asset_file_url(canonical_path: &Path, revision: &str) -> String {
    format!(
        "file://{}?v={}",
        crate::config::encode_path(&canonical_path.to_string_lossy()),
        revision
    )
}

/// FNV-1a 64-bit hash state (zero external dependencies).
struct Fnv64(u64);

impl Fnv64 {
    fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }

    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 ^= u64::from(b);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }

    fn write_file(&mut self, path: &Path) {
        self.write(path.to_string_lossy().as_bytes());
        self.write(&[0xff]);
        if let Ok(meta) = std::fs::metadata(path) {
            self.write(&meta.len().to_le_bytes());
            if let Ok(modified) = meta.modified() {
                if let Ok(dur) = modified.duration_since(std::time::UNIX_EPOCH) {
                    self.write(&dur.as_secs().to_le_bytes());
                    self.write(&dur.subsec_nanos().to_le_bytes());
                }
            }
            if meta.len() <= MAX_HASH_BYTES {
                if let Ok(contents) = std::fs::read(path) {
                    self.write(&contents);
                }
            }
        }
        self.write(&[0xfe]);
    }

    fn finish_hex(self) -> String {
        format!("{:016x}", self.0)
    }
}

/// Discover and validate all enabled user shell extensions across `dirs`.
///
/// Invalid manifests, escaping symlinks, missing scripts, or unknown slots are
/// logged and skipped individually so one broken extension never breaks the
/// rest or prevents the compositor from starting.
///
/// `force_gen` > 0 appends a reload counter to the revision of `force_id` (or
/// all extensions when `force_id` is `None`) so explicit `extensions.reload`
/// requests always trigger a fresh load in the shell even when bytes did not
/// change.
pub fn discover(
    dirs: &[PathBuf],
    policy: &Policy,
    force_gen: u64,
    force_id: Option<&str>,
) -> Vec<ShellExtension> {
    if !policy.enabled {
        return Vec::new();
    }

    let mut found: Vec<ShellExtension> = Vec::new();
    let mut seen_ids: HashSet<String> = HashSet::new();

    for root in dirs {
        let Ok(canonical_root) = std::fs::canonicalize(root) else {
            continue;
        };
        let Ok(entries) = std::fs::read_dir(&canonical_root) else {
            continue;
        };
        let mut child_paths: Vec<PathBuf> = entries
            .flatten()
            .filter(|entry| {
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                !name_str.starts_with('.')
            })
            .map(|entry| entry.path())
            .collect();
        child_paths.sort();

        for child in child_paths {
            let Ok(canonical_ext_dir) = std::fs::canonicalize(&child) else {
                continue;
            };
            if !canonical_ext_dir.is_dir() {
                continue;
            }
            // Reject directory symlinks that escape the configured extensions root.
            if !canonical_ext_dir.starts_with(&canonical_root) {
                tracing::warn!(
                    "shell extension: skipping {} (escapes extensions directory {})",
                    child.display(),
                    canonical_root.display()
                );
                continue;
            }

            let manifest_rel = "extension.json";
            let manifest_path = match verify_asset_path(&canonical_ext_dir, manifest_rel) {
                Ok(path) => path,
                Err(_) => {
                    // Subdirectory without extension.json: silently ignore unless
                    // extension.json exists and failed containment/stat.
                    if canonical_ext_dir
                        .join(manifest_rel)
                        .symlink_metadata()
                        .is_ok()
                    {
                        tracing::warn!(
                            "shell extension: skipping {} (invalid or escaping extension.json)",
                            canonical_ext_dir.display()
                        );
                    }
                    continue;
                }
            };

            let manifest_len = std::fs::metadata(&manifest_path)
                .map(|m| m.len())
                .unwrap_or(u64::MAX);
            if manifest_len > MAX_MANIFEST_BYTES {
                tracing::warn!(
                    "shell extension: skipping {} (extension.json exceeds {} bytes)",
                    canonical_ext_dir.display(),
                    MAX_MANIFEST_BYTES
                );
                continue;
            }

            let text = match std::fs::read_to_string(&manifest_path) {
                Ok(text) => text,
                Err(e) => {
                    tracing::warn!(
                        "shell extension: skipping {} (cannot read extension.json: {e})",
                        canonical_ext_dir.display()
                    );
                    continue;
                }
            };

            let manifest: Manifest = match serde_json::from_str(&text) {
                Ok(manifest) => manifest,
                Err(e) => {
                    tracing::warn!(
                        "shell extension: skipping {} (invalid extension.json: {e})",
                        canonical_ext_dir.display()
                    );
                    continue;
                }
            };

            if !valid_id(&manifest.id) {
                tracing::warn!(
                    "shell extension: skipping {} (invalid id {:?})",
                    canonical_ext_dir.display(),
                    manifest.id
                );
                continue;
            }

            // Precedence: disable > enable > manifest.enabled (default true).
            if policy.disable.iter().any(|id| id == &manifest.id) {
                continue;
            }
            let active = policy.enable.iter().any(|id| id == &manifest.id) || manifest.enabled;
            if !active {
                continue;
            }

            if !seen_ids.insert(manifest.id.clone()) {
                tracing::warn!(
                    "shell extension: duplicate id {:?} in {}; keeping earlier definition",
                    manifest.id,
                    canonical_ext_dir.display()
                );
                continue;
            }

            let script_path = match verify_asset_path(&canonical_ext_dir, &manifest.script) {
                Ok(path) => path,
                Err(e) => {
                    tracing::warn!("shell extension {:?}: invalid script: {e}", manifest.id);
                    continue;
                }
            };

            let style_path = match manifest.style.as_deref().map(str::trim) {
                Some("") | None => None,
                Some(style_rel) => match verify_asset_path(&canonical_ext_dir, style_rel) {
                    Ok(path) => Some(path),
                    Err(e) => {
                        tracing::warn!("shell extension {:?}: invalid style: {e}", manifest.id);
                        continue;
                    }
                },
            };

            let mut hasher = Fnv64::new();
            hasher.write_file(&manifest_path);
            hasher.write_file(&script_path);
            if let Some(style) = &style_path {
                hasher.write_file(style);
            }
            let mut revision = hasher.finish_hex();
            if force_gen > 0 && (force_id.is_none() || force_id == Some(manifest.id.as_str())) {
                revision.push('-');
                revision.push_str(&force_gen.to_string());
            }

            let script = asset_file_url(&script_path, &revision);
            let style = style_path
                .as_deref()
                .map(|path| asset_file_url(path, &revision));
            let name = manifest
                .name
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(&manifest.id)
                .to_owned();

            found.push(ShellExtension {
                id: manifest.id,
                name,
                slot: manifest.slot,
                script,
                style,
                revision,
                version: manifest.version,
                description: manifest.description,
                options: manifest.options,
            });
        }
    }

    found.sort_by(|a, b| a.id.cmp(&b.id));
    found
}

/// Compute `(changed, removed)` extension IDs between two sorted snapshots.
pub fn diff(previous: &[ShellExtension], next: &[ShellExtension]) -> (Vec<String>, Vec<String>) {
    let prev_map: HashMap<&str, &ShellExtension> =
        previous.iter().map(|ext| (ext.id.as_str(), ext)).collect();
    let next_map: HashMap<&str, &ShellExtension> =
        next.iter().map(|ext| (ext.id.as_str(), ext)).collect();

    let mut changed: Vec<String> = next
        .iter()
        .filter(|ext| prev_map.get(ext.id.as_str()).copied() != Some(*ext))
        .map(|ext| ext.id.clone())
        .collect();
    changed.sort();

    let mut removed: Vec<String> = previous
        .iter()
        .filter(|ext| !next_map.contains_key(ext.id.as_str()))
        .map(|ext| ext.id.clone())
        .collect();
    removed.sort();

    (changed, removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn temp_root(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "viewport-ext-test-{tag}-{}-{nanos}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn discovers_valid_extension_with_script_style_and_options() {
        let root = temp_root("valid");
        let ext = root.join("workspace-pill");
        std::fs::create_dir_all(&ext).unwrap();
        std::fs::write(
            ext.join("extension.json"),
            r#"{
              "id": "workspace-pill",
              "name": "Workspace Pill",
              "version": "1.0.0",
              "description": "Test pill",
              "slot": "bar-center",
              "script": "index.js",
              "style": "style.css",
              "options": { "prefix": "WS" }
            }"#,
        )
        .unwrap();
        std::fs::write(ext.join("index.js"), "console.log('hi');").unwrap();
        std::fs::write(ext.join("style.css"), ".pill { color: red; }").unwrap();

        let list = discover(std::slice::from_ref(&root), &Policy::default(), 0, None);
        assert_eq!(list.len(), 1);
        let item = &list[0];
        assert_eq!(item.id, "workspace-pill");
        assert_eq!(item.name, "Workspace Pill");
        assert_eq!(item.slot, ExtensionSlot::BarCenter);
        assert!(item.script.starts_with("file://"));
        assert!(item.script.contains("index.js?v="));
        assert!(item.style.as_deref().unwrap().contains("style.css?v="));
        assert_eq!(item.version.as_deref(), Some("1.0.0"));
        assert_eq!(item.options, Some(serde_json::json!({ "prefix": "WS" })));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn blocks_path_traversal_remote_urls_and_escaping_symlinks() {
        let root = temp_root("security");
        let outside = root.join("secret.js");
        std::fs::write(&outside, "secret").unwrap();
        let exts_root = root.join("extensions");
        std::fs::create_dir_all(&exts_root).unwrap();

        // 1. ../ traversal
        let ext_dotdot = exts_root.join("dotdot");
        std::fs::create_dir_all(&ext_dotdot).unwrap();
        std::fs::write(
            ext_dotdot.join("extension.json"),
            r#"{"id":"dotdot","slot":"bar-right","script":"../../secret.js"}"#,
        )
        .unwrap();

        // 2. absolute path
        let ext_abs = exts_root.join("abs");
        std::fs::create_dir_all(&ext_abs).unwrap();
        std::fs::write(
            ext_abs.join("extension.json"),
            format!(
                r#"{{"id":"abs","slot":"bar-right","script":"{}"}}"#,
                outside.display()
            ),
        )
        .unwrap();

        // 3. remote URL
        let ext_url = exts_root.join("remote");
        std::fs::create_dir_all(&ext_url).unwrap();
        std::fs::write(
            ext_url.join("extension.json"),
            r#"{"id":"remote","slot":"bar-right","script":"https://evil.example/a.js"}"#,
        )
        .unwrap();

        // 4. symlink inside extension dir pointing outside
        let ext_sym = exts_root.join("sym");
        std::fs::create_dir_all(&ext_sym).unwrap();
        symlink(&outside, ext_sym.join("escape.js")).unwrap();
        std::fs::write(
            ext_sym.join("extension.json"),
            r#"{"id":"sym","slot":"bar-right","script":"escape.js"}"#,
        )
        .unwrap();

        // 5. escaping style symlink
        let ext_style_sym = exts_root.join("stylesym");
        std::fs::create_dir_all(&ext_style_sym).unwrap();
        std::fs::write(ext_style_sym.join("ok.js"), "// ok").unwrap();
        symlink(&outside, ext_style_sym.join("escape.css")).unwrap();
        std::fs::write(
            ext_style_sym.join("extension.json"),
            r#"{"id":"stylesym","slot":"desktop","script":"ok.js","style":"escape.css"}"#,
        )
        .unwrap();

        // 6. directory symlink escaping extensions root
        let outside_ext = root.join("outside-ext");
        std::fs::create_dir_all(&outside_ext).unwrap();
        std::fs::write(outside_ext.join("ok.js"), "// ok").unwrap();
        std::fs::write(
            outside_ext.join("extension.json"),
            r#"{"id":"escaped-dir","slot":"overlay","script":"ok.js"}"#,
        )
        .unwrap();
        symlink(&outside_ext, exts_root.join("escaped-dir")).unwrap();

        let list = discover(
            std::slice::from_ref(&exts_root),
            &Policy::default(),
            0,
            None,
        );
        assert!(
            list.is_empty(),
            "all traversal and symlink escapes must be rejected, got {list:?}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn policy_enable_disable_and_master_switch_precedence() {
        let root = temp_root("policy");
        for (id, enabled) in [("alpha", true), ("beta", false), ("gamma", true)] {
            let dir = root.join(id);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("index.js"), "// js").unwrap();
            std::fs::write(
                dir.join("extension.json"),
                format!(
                    r#"{{"id":"{id}","slot":"bar-left","script":"index.js","enabled":{enabled}}}"#
                ),
            )
            .unwrap();
        }

        // Default: alpha + gamma
        let ids = |policy: &Policy| -> Vec<String> {
            discover(std::slice::from_ref(&root), policy, 0, None)
                .into_iter()
                .map(|e| e.id)
                .collect()
        };
        assert_eq!(ids(&Policy::default()), vec!["alpha", "gamma"]);

        // Master disabled: empty
        assert!(ids(&Policy {
            enabled: false,
            ..Default::default()
        })
        .is_empty());

        // enable overrides manifest enabled:false; disable wins over enable
        assert_eq!(
            ids(&Policy {
                enabled: true,
                enable: vec!["beta".into(), "alpha".into()],
                disable: vec!["alpha".into()],
            }),
            vec!["beta", "gamma"]
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn revision_changes_on_edit_and_diff_isolates_changed_and_removed() {
        let root = temp_root("diff");
        for id in ["one", "two"] {
            let dir = root.join(id);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("index.js"), format!("// {id} v1")).unwrap();
            std::fs::write(
                dir.join("extension.json"),
                format!(r#"{{"id":"{id}","slot":"bar-center","script":"index.js"}}"#),
            )
            .unwrap();
        }

        let first = discover(std::slice::from_ref(&root), &Policy::default(), 0, None);
        assert_eq!(first.len(), 2);

        // Edit only "one" and remove "two"
        std::fs::write(root.join("one/index.js"), "// one v2").unwrap();
        std::fs::remove_dir_all(root.join("two")).unwrap();

        let second = discover(std::slice::from_ref(&root), &Policy::default(), 0, None);
        let (changed, removed) = diff(&first, &second);
        assert_eq!(changed, vec!["one"]);
        assert_eq!(removed, vec!["two"]);
        assert_ne!(first[0].revision, second[0].revision);

        // Force reload bumps revision even without file edits
        let forced = discover(
            std::slice::from_ref(&root),
            &Policy::default(),
            1,
            Some("one"),
        );
        let (changed_forced, removed_forced) = diff(&second, &forced);
        assert_eq!(changed_forced, vec!["one"]);
        assert!(removed_forced.is_empty());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rejects_invalid_ids() {
        for bad in [
            "", ".", "..", "../a", "a/b", "bad name", "-leading", "_lead",
        ] {
            assert!(!valid_id(bad), "should reject {bad:?}");
        }
        for ok in ["a", "hello-world", "clock_2", "Ext123"] {
            assert!(valid_id(ok), "should accept {ok:?}");
        }
    }
}
