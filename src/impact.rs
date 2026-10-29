//! Static dependency and blast-radius (affected files) analyzer for Rust and TypeScript.
//!
//! Given a repository checkout and a modified file (plus optional unified diff),
//! resolves module imports/uses across the repository and computes both direct
//! and transitive reverse dependencies (`affected_files`) as well as public/exported
//! symbols touched by the edit (`touched_symbols`).

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::path::{Component, Path};

use serde::{Deserialize, Serialize};

mod rust;
mod typescript;
use rust::{RustWorkspaceIndex, collect_cargo_tomls, resolve_rust_dependencies};
use typescript::{TsConfigAliases, resolve_ts_dependencies};

/// Supported source language for impact analysis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceLanguage {
    Rust,
    TypeScript,
}

impl SourceLanguage {
    pub fn from_path(path: &str) -> Option<Self> {
        let ext = Path::new(path).extension()?.to_str()?;
        match ext {
            "rs" => Some(Self::Rust),
            "ts" | "tsx" | "mts" | "cts" | "js" | "jsx" | "mjs" | "cjs" => Some(Self::TypeScript),
            _ => None,
        }
    }
}

/// Detailed impact report for a single file in a repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileImpactReport {
    /// Repo-relative path using forward slashes.
    pub path: String,
    /// Detected language (`rust` or `typescript`), if supported.
    pub language: Option<SourceLanguage>,
    /// Public/exported symbols defined in this file (or touched by the diff).
    pub exported_symbols: Vec<String>,
    /// Public/exported symbols touched by the current diff.
    pub touched_symbols: Vec<String>,
    /// Files that this file directly imports/depends on.
    pub dependencies: Vec<String>,
    /// Files that directly import/depend on this file (distance = 1).
    pub direct_dependents: Vec<String>,
    /// All files transitively affected when this file is modified (distance >= 1).
    pub affected_files: Vec<String>,
    /// Static analysis is conservative and cannot prove complete semantic impact.
    pub limitations: Vec<String>,
}

/// Repository-wide dependency graph for Rust and TypeScript files.
#[derive(Debug, Clone, Default)]
pub struct RepoImpactGraph {
    /// Forward edges: `file -> files it imports/depends on`.
    pub dependencies: BTreeMap<String, BTreeSet<String>>,
    /// Reverse edges: `file -> files that import/depend on it`.
    pub dependents: BTreeMap<String, BTreeSet<String>>,
    /// Exported/public symbols per file.
    pub exports: BTreeMap<String, Vec<String>>,
}

/// Per-checkout cache. Read and parse changed sources only; changing the set of
/// paths or resolver configuration invalidates resolution for all importers.
#[derive(Default)]
pub struct RepoImpactCache {
    stamps: BTreeMap<String, Option<(std::time::SystemTime, u64)>>,
    contents: HashMap<String, String>,
    config: Vec<(String, Option<(std::time::SystemTime, u64)>)>,
    head: Vec<u8>,
    graph: RepoImpactGraph,
}

fn file_stamp(path: &Path) -> Option<(std::time::SystemTime, u64)> {
    let metadata = std::fs::metadata(path).ok()?;
    Some((metadata.modified().ok()?, metadata.len()))
}

impl RepoImpactCache {
    pub fn graph(&self) -> &RepoImpactGraph {
        &self.graph
    }
    pub fn refresh(&mut self, root: &Path) -> &RepoImpactGraph {
        let mut files = Vec::new();
        collect_source_files(root, root, &mut files);
        // Keep deleted tracked files addressable, using their HEAD content. This
        // also works on the first scan after startup and for staged deletions.
        if let Ok(output) = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["ls-files", "-z"])
            .output()
            && output.status.success()
        {
            for path in output
                .stdout
                .split(|b| *b == 0)
                .filter_map(|p| std::str::from_utf8(p).ok())
            {
                if SourceLanguage::from_path(path).is_some() {
                    files.push(path.to_owned());
                }
            }
        }
        files.sort();
        files.dedup();
        let file_set: BTreeSet<String> = files.into_iter().collect();
        let stamps: BTreeMap<_, _> = file_set
            .iter()
            .map(|p| (p.clone(), file_stamp(&root.join(p))))
            .collect();
        let mut configs = Vec::new();
        collect_cargo_tomls(root, root, &mut configs);
        configs.extend(["tsconfig.json", "tsconfig.base.json", "jsconfig.json"].map(str::to_owned));
        configs.sort();
        let config = configs
            .into_iter()
            .map(|p| {
                let stamp = file_stamp(&root.join(&p));
                (p, stamp)
            })
            .collect::<Vec<_>>();
        let head = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["rev-parse", "HEAD"])
            .output()
            .map(|o| o.stdout)
            .unwrap_or_default();
        let all =
            self.config != config || self.head != head || self.stamps.keys().ne(stamps.keys());
        let changed: Vec<_> = file_set
            .iter()
            .filter(|p| all || self.stamps.get(*p) != stamps.get(*p))
            .cloned()
            .collect();
        if changed.is_empty() {
            return &self.graph;
        }
        self.contents.retain(|p, _| file_set.contains(p));
        for path in &changed {
            let content = std::fs::read_to_string(root.join(path)).ok().or_else(|| {
                let out = std::process::Command::new("git")
                    .arg("-C")
                    .arg(root)
                    .args(["show", &format!("HEAD:{path}")])
                    .output()
                    .ok()?;
                out.status
                    .success()
                    .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
            });
            if let Some(content) = content {
                self.contents.insert(path.clone(), content);
            } else {
                self.contents.remove(path);
            }
        }
        let aliases = TsConfigAliases::load(root);
        let rust_index = RustWorkspaceIndex::build(root, &file_set, &self.contents);
        self.graph.dependencies.retain(|p, _| file_set.contains(p));
        self.graph.exports.retain(|p, _| file_set.contains(p));
        for path in changed {
            let Some(content) = self.contents.get(&path) else {
                continue;
            };
            let Some(lang) = SourceLanguage::from_path(&path) else {
                continue;
            };
            self.graph
                .exports
                .insert(path.clone(), extract_exported_symbols(lang, content));
            let mut deps = match lang {
                SourceLanguage::TypeScript => {
                    resolve_ts_dependencies(&path, content, &file_set, &aliases)
                }
                SourceLanguage::Rust => {
                    resolve_rust_dependencies(&path, content, &file_set, &rust_index)
                }
            };
            deps.remove(&path);
            self.graph.dependencies.insert(path, deps);
        }
        self.graph.dependents.clear();
        for (path, deps) in &self.graph.dependencies {
            for dep in deps {
                self.graph
                    .dependents
                    .entry(dep.clone())
                    .or_default()
                    .insert(path.clone());
            }
        }
        self.stamps = stamps;
        self.config = config;
        self.head = head;
        &self.graph
    }
}

impl RepoImpactGraph {
    /// Build a static dependency graph for all Rust and TypeScript files in `repo_root`.
    pub fn build(repo_root: &Path) -> Self {
        RepoImpactCache::default().refresh(repo_root).clone()
    }

    /// Compute the transitive blast radius (all affected files) for `path`.
    pub fn affected_files(&self, path: &str) -> Vec<String> {
        let normalized = normalize_rel_path(path);
        let mut visited = BTreeSet::new();
        let mut queue = VecDeque::new();

        if let Some(direct) = self.dependents.get(&normalized) {
            for dep in direct {
                if dep != &normalized && visited.insert(dep.clone()) {
                    queue.push_back(dep.clone());
                }
            }
        }

        while let Some(curr) = queue.pop_front() {
            if let Some(next_deps) = self.dependents.get(&curr) {
                for next in next_deps {
                    if next != &normalized && visited.insert(next.clone()) {
                        queue.push_back(next.clone());
                    }
                }
            }
        }

        visited.into_iter().collect()
    }

    /// Generate a full impact report for `path`, optionally incorporating a unified `diff`.
    pub fn report_for(&self, repo_root: &Path, path: &str, diff: Option<&str>) -> FileImpactReport {
        let normalized = normalize_rel_path(path);
        let language = SourceLanguage::from_path(&normalized);
        let content = std::fs::read_to_string(repo_root.join(&normalized)).ok();

        let exported_symbols = self
            .exports
            .get(&normalized)
            .cloned()
            .or_else(|| {
                language
                    .zip(content.as_deref())
                    .map(|(lang, text)| extract_exported_symbols(lang, text))
            })
            .unwrap_or_default();

        let touched_symbols = match (language, diff) {
            (Some(lang), Some(d)) if !d.is_empty() => {
                extract_touched_symbols(lang, d, content.as_deref(), &exported_symbols)
            }
            _ => exported_symbols.clone(),
        };

        let dependencies = self
            .dependencies
            .get(&normalized)
            .map(|s| s.iter().cloned().collect())
            .unwrap_or_default();

        let direct_dependents = self
            .dependents
            .get(&normalized)
            .map(|s| s.iter().cloned().collect())
            .unwrap_or_default();

        let affected_files = self.affected_files(&normalized);

        FileImpactReport {
            path: normalized,
            language,
            exported_symbols,
            touched_symbols,
            dependencies,
            direct_dependents,
            affected_files,
            limitations: vec!["Static module analysis; generated code, macros, dynamic specifiers, package exports and inherited/nested TS configs may require compiler/LSP analysis.".into()],
        }
    }
}

fn should_skip_dir(name: &str) -> bool {
    matches!(
        name,
        ".git"
            | "target"
            | "node_modules"
            | "dist"
            | "build"
            | ".next"
            | ".nuxt"
            | ".vitepress"
            | "coverage"
            | "out"
            | ".buddies"
            | ".cargo"
    )
}

fn collect_source_files(root: &Path, current: &Path, out: &mut Vec<String>) {
    let Ok(read_dir) = std::fs::read_dir(current) else {
        return;
    };
    for entry in read_dir.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            if let Some(name) = path.file_name().and_then(|n| n.to_str())
                && !should_skip_dir(name)
            {
                collect_source_files(root, &path, out);
            }
        } else if file_type.is_file()
            && let Ok(rel) = path.strip_prefix(root)
        {
            let rel_str = normalize_path_buf(rel);
            if SourceLanguage::from_path(&rel_str).is_some() {
                out.push(rel_str);
            }
        }
    }
}

pub fn normalize_rel_path(path: &str) -> String {
    normalize_path_buf(Path::new(path))
}

fn normalize_path_buf(path: &Path) -> String {
    let mut parts: Vec<String> = Vec::new();
    for comp in path.components() {
        match comp {
            Component::Normal(s) => parts.push(s.to_string_lossy().into_owned()),
            Component::ParentDir => {
                parts.pop();
            }
            Component::CurDir | Component::RootDir | Component::Prefix(_) => {}
        }
    }
    parts.join("/")
}

fn strip_c_style_comments(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    let mut string_delim: Option<u8> = None;

    while i < bytes.len() {
        if let Some(delim) = string_delim {
            if bytes[i] == b'\\' && i + 1 < bytes.len() {
                out.push('\\');
                i += 1;
                let ch = input[i..].chars().next().unwrap();
                out.push(ch);
                i += ch.len_utf8();
                continue;
            }
            if bytes[i] == delim {
                string_delim = None;
            }
            let ch = input[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
            continue;
        }

        if matches!(bytes[i], b'"' | b'\'' | b'`') {
            string_delim = Some(bytes[i]);
            let ch = input[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
            continue;
        }

        if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'/' {
            i += 2;
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }

        if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                if bytes[i] == b'\n' {
                    out.push('\n');
                }
                i += 1;
            }
            i = (i + 2).min(bytes.len());
            continue;
        }

        let ch = input[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

// ============================================================================
// Exported / Public Symbol Extraction & Diff Matching
// ============================================================================

pub fn extract_exported_symbols(lang: SourceLanguage, content: &str) -> Vec<String> {
    let clean = strip_c_style_comments(content);
    let mut symbols = BTreeSet::new();

    match lang {
        SourceLanguage::Rust => {
            for line in clean.lines() {
                let trimmed = line.trim();
                if !trimmed.starts_with("pub") && !trimmed.starts_with("macro_rules!") {
                    continue;
                }
                if let Some(rest) = trimmed.strip_prefix("macro_rules!") {
                    if let Some(ident) = first_ident(rest.trim()) {
                        symbols.insert(ident);
                    }
                    continue;
                }
                let after_pub = strip_rust_visibility(trimmed);
                if after_pub.len() == trimmed.len() {
                    continue;
                }
                let after_qualifiers = after_pub
                    .trim_start_matches("async ")
                    .trim_start_matches("const ")
                    .trim_start_matches("unsafe ")
                    .trim_start();
                for kw in [
                    "fn ", "struct ", "enum ", "trait ", "type ", "const ", "static ", "mod ",
                ] {
                    if let Some(rest) = after_qualifiers.strip_prefix(kw)
                        && let Some(ident) = first_ident(rest.trim())
                    {
                        symbols.insert(ident);
                    }
                }
            }
        }
        SourceLanguage::TypeScript => {
            for line in clean.lines() {
                let trimmed = line.trim();
                let Some(after_export) = trimmed.strip_prefix("export ") else {
                    continue;
                };
                let rest = after_export
                    .trim_start()
                    .trim_start_matches("default ")
                    .trim_start_matches("declare ")
                    .trim_start_matches("abstract ")
                    .trim_start_matches("async ")
                    .trim_start();

                for kw in [
                    "function ",
                    "function*",
                    "class ",
                    "interface ",
                    "type ",
                    "enum ",
                    "const ",
                    "let ",
                    "var ",
                ] {
                    if let Some(after_kw) = rest.strip_prefix(kw)
                        && let Some(ident) = first_ident(after_kw.trim())
                    {
                        symbols.insert(ident);
                    }
                }
                if let Some(brace_start) = rest.strip_prefix('{')
                    && let Some((inside, _)) = brace_start.split_once('}')
                {
                    for part in inside.split(',') {
                        let exported = part
                            .split(" as ")
                            .last()
                            .unwrap_or(part)
                            .trim()
                            .trim_start_matches("type ")
                            .trim();
                        if let Some(ident) = first_ident(exported) {
                            symbols.insert(ident);
                        }
                    }
                }
            }
        }
    }

    symbols.into_iter().collect()
}

fn extract_touched_symbols(
    lang: SourceLanguage,
    diff: &str,
    _current_content: Option<&str>,
    exported_symbols: &[String],
) -> Vec<String> {
    let mut touched = BTreeSet::new();
    let mut hunk_context_symbols = BTreeSet::new();
    let mut hunk_has_changes = false;

    for line in diff.lines() {
        // Unified diff hunk header often contains enclosing function/struct context: `@@ -10,4 +10,6 @@ pub fn foo(...)`
        if let Some((_, context)) = line.split_once(" @@") {
            if hunk_has_changes {
                touched.extend(std::mem::take(&mut hunk_context_symbols));
            } else {
                hunk_context_symbols.clear();
            }
            hunk_has_changes = false;

            let ctx = context.trim();
            for sym in extract_exported_symbols(lang, ctx) {
                touched.insert(sym);
            }
            for exp in exported_symbols {
                if contains_ident(ctx, exp) {
                    touched.insert(exp.clone());
                }
            }
            continue;
        }
        if line.starts_with("+++") || line.starts_with("---") {
            continue;
        }
        if let Some(changed) = line.strip_prefix('+').or_else(|| line.strip_prefix('-')) {
            hunk_has_changes = true;
            for sym in extract_exported_symbols(lang, changed) {
                touched.insert(sym);
            }
            for exp in exported_symbols {
                if contains_ident(changed, exp) {
                    touched.insert(exp.clone());
                }
            }
        } else if let Some(ctx_line) = line.strip_prefix(' ') {
            for sym in extract_exported_symbols(lang, ctx_line) {
                hunk_context_symbols.insert(sym);
            }
            for exp in exported_symbols {
                if contains_ident(ctx_line, exp) {
                    hunk_context_symbols.insert(exp.clone());
                }
            }
        }
    }

    if hunk_has_changes {
        touched.extend(hunk_context_symbols);
    }

    touched.into_iter().collect()
}

fn strip_rust_visibility(s: &str) -> &str {
    let Some(rest) = s.strip_prefix("pub") else {
        return s;
    };
    let rest = rest.trim_start();
    if let Some(after_open) = rest.strip_prefix('(')
        && let Some((_, after_close)) = after_open.split_once(')')
    {
        return after_close.trim_start();
    }
    rest
}

fn first_ident(s: &str) -> Option<String> {
    let s = s.trim_start_matches('*').trim_start();
    let mut end = 0;
    for (i, c) in s.char_indices() {
        if i == 0 {
            if !(c.is_ascii_alphabetic() || c == '_' || c == '$') {
                return None;
            }
        } else if !(c.is_ascii_alphanumeric() || c == '_' || c == '$') {
            break;
        }
        end = i + c.len_utf8();
    }
    if end > 0 {
        Some(s[..end].to_string())
    } else {
        None
    }
}

fn is_valid_ident(s: &str) -> bool {
    first_ident(s).is_some_and(|id| id == s)
}

fn contains_ident(haystack: &str, ident: &str) -> bool {
    if ident.is_empty() {
        return false;
    }
    let bytes = haystack.as_bytes();
    let mut start = 0;
    while let Some(pos) = haystack[start..].find(ident) {
        let idx = start + pos;
        let end = idx + ident.len();
        let before_ok =
            idx == 0 || (!bytes[idx - 1].is_ascii_alphanumeric() && bytes[idx - 1] != b'_');
        let after_ok =
            end >= bytes.len() || (!bytes[end].is_ascii_alphanumeric() && bytes[end] != b'_');
        if before_ok && after_ok {
            return true;
        }
        start = idx + 1;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_impact_graph_computes_direct_and_transitive_affected_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();

        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("src/services")).unwrap();

        std::fs::write(
            root.join("src/lib.rs"),
            "pub mod models;\npub mod services;\npub mod api;\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src/models.rs"),
            "pub struct User {\n    pub id: u64,\n}\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src/services/mod.rs"),
            "use crate::models::User;\npub fn load_user(id: u64) -> User { User { id } }\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src/api.rs"),
            "use crate::services::load_user;\npub fn endpoint() -> u64 { load_user(1).id }\n",
        )
        .unwrap();

        let graph = RepoImpactGraph::build(root);
        let report = graph.report_for(
            root,
            "src/models.rs",
            Some("--- a/src/models.rs\n+++ b/src/models.rs\n@@ -1,2 +1,3 @@\n pub struct User {\n     pub id: u64,\n+    pub email: String,\n }\n"),
        );

        assert_eq!(report.language, Some(SourceLanguage::Rust));
        assert!(report.exported_symbols.contains(&"User".to_string()));
        assert!(report.touched_symbols.contains(&"User".to_string()));
        assert!(
            report
                .direct_dependents
                .contains(&"src/services/mod.rs".to_string())
        );
        assert!(
            report
                .affected_files
                .contains(&"src/services/mod.rs".to_string())
        );
        assert!(report.affected_files.contains(&"src/api.rs".to_string()));
        assert!(report.affected_files.contains(&"src/lib.rs".to_string()));
    }

    #[test]
    fn typescript_impact_graph_resolves_relative_and_tsconfig_alias_imports() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();

        std::fs::write(
            root.join("tsconfig.json"),
            r#"{
                // JSONC comment
                "compilerOptions": {
                    "baseUrl": ".",
                    "paths": {
                        "@/*": ["src/*"],
                    }
                }
            }"#,
        )
        .unwrap();

        std::fs::create_dir_all(root.join("src/components")).unwrap();
        std::fs::write(
            root.join("src/types.ts"),
            "export interface SessionConfig {\n  token: string;\n}\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src/client.ts"),
            "import type { SessionConfig } from './types.js';\nexport function connect(cfg: SessionConfig) { return cfg.token; }\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src/components/App.tsx"),
            "import { connect } from '@/client';\nexport const App = () => connect({ token: 'abc' });\n",
        )
        .unwrap();

        let graph = RepoImpactGraph::build(root);
        let report = graph.report_for(root, "src/types.ts", Some("+  sprint?: string;\n"));

        assert_eq!(report.language, Some(SourceLanguage::TypeScript));
        assert_eq!(report.exported_symbols, vec!["SessionConfig".to_string()]);
        assert_eq!(report.direct_dependents, vec!["src/client.ts".to_string()]);
        assert_eq!(
            report.affected_files,
            vec![
                "src/client.ts".to_string(),
                "src/components/App.tsx".to_string()
            ]
        );
    }

    #[test]
    fn multiline_unicode_imports_and_incremental_updates() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("caffè.ts"), "export const x=1;\n").unwrap();
        std::fs::write(
            root.join("b.ts"),
            "import {\n x\n} from './caffè';\nexport {\n x\n} from './caffè';\n",
        )
        .unwrap();
        let mut cache = RepoImpactCache::default();
        assert_eq!(cache.refresh(root).affected_files("caffè.ts"), vec!["b.ts"]);
        std::fs::write(root.join("b.ts"), "export const unrelated = 42;\n").unwrap();
        assert!(cache.refresh(root).affected_files("caffè.ts").is_empty());
        std::fs::write(root.join("b.ts"), "const m = import ( './caffè' );\n").unwrap();
        assert_eq!(cache.refresh(root).affected_files("caffè.ts"), vec!["b.ts"]);
    }
    #[test]
    fn deleted_tracked_sources_keep_dependents_on_first_scan() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for args in [
            vec!["init", "-q"],
            vec!["config", "commit.gpgsign", "false"],
            vec!["config", "user.name", "test"],
            vec!["config", "user.email", "test@example.invalid"],
        ] {
            assert!(
                std::process::Command::new("git")
                    .arg("-C")
                    .arg(root)
                    .args(args)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        std::fs::write(root.join("a.ts"), "export const x=1;\n").unwrap();
        std::fs::write(root.join("b.ts"), "import {x} from './a';\n").unwrap();
        for args in [vec!["add", "."], vec!["commit", "-qm", "initial"]] {
            assert!(
                std::process::Command::new("git")
                    .arg("-C")
                    .arg(root)
                    .args(args)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        std::fs::remove_file(root.join("a.ts")).unwrap();
        assert_eq!(
            RepoImpactGraph::build(root).affected_files("a.ts"),
            vec!["b.ts"]
        );
    }
    #[test]
    fn crate_root_items_and_reexports_affect_consumers() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join("src")).unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname='demo'\nversion='0.1.0'\n",
        )
        .unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub struct Config;\nmod api;\n").unwrap();
        std::fs::write(
            root.join("src/api.rs"),
            "use crate::Config;\npub fn f(_: Config) {}\n",
        )
        .unwrap();
        assert!(
            RepoImpactGraph::build(root)
                .affected_files("src/lib.rs")
                .contains(&"src/api.rs".into())
        );
    }
}
