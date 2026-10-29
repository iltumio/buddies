use super::*;

// ============================================================================
// Rust Module & Use-Path Resolution
// ============================================================================

#[derive(Debug, Clone)]
struct RustCrateInfo {
    name: String,
    root_dir: String,
    entry_files: Vec<String>,
    /// `module_path (e.g. "orchestrator::state") -> file_path ("src/orchestrator/state.rs")`
    module_to_file: HashMap<String, String>,
    /// `file_path -> module_path (e.g. "" for lib.rs/main.rs, "orchestrator" for src/orchestrator/mod.rs)`
    file_to_module: HashMap<String, String>,
    /// Parent module file -> child module files declared via `mod foo;`
    mod_children: HashMap<String, BTreeSet<String>>,
}

#[derive(Debug, Clone, Default)]
pub(super) struct RustWorkspaceIndex {
    crates: Vec<RustCrateInfo>,
    /// `crate_ident (with underscores) -> crate index`
    by_name: HashMap<String, usize>,
}

impl RustWorkspaceIndex {
    pub(super) fn build(
        repo_root: &Path,
        file_set: &BTreeSet<String>,
        contents: &HashMap<String, String>,
    ) -> Self {
        let mut cargo_tomls = Vec::new();
        collect_cargo_tomls(repo_root, repo_root, &mut cargo_tomls);
        cargo_tomls.sort();

        let mut crates = Vec::new();
        for toml_rel in cargo_tomls {
            let toml_path = repo_root.join(&toml_rel);
            let Ok(raw) = std::fs::read_to_string(&toml_path) else {
                continue;
            };
            let Ok(parsed) = raw.parse::<toml::Table>() else {
                continue;
            };
            let Some(pkg) = parsed.get("package").and_then(|v| v.as_table()) else {
                continue;
            };
            let Some(pkg_name) = pkg.get("name").and_then(|v| v.as_str()) else {
                continue;
            };
            let crate_ident = pkg_name.replace('-', "_");
            let root_dir = Path::new(&toml_rel)
                .parent()
                .map(normalize_path_buf)
                .unwrap_or_default();

            let prefix = if root_dir.is_empty() {
                String::new()
            } else {
                format!("{root_dir}/")
            };

            let mut entry_files = Vec::new();
            if let Some(lib_path) = parsed
                .get("lib")
                .and_then(|v| v.as_table())
                .and_then(|t| t.get("path"))
                .and_then(|v| v.as_str())
            {
                let rel = normalize_rel_path(&format!("{prefix}{lib_path}"));
                if file_set.contains(&rel) {
                    entry_files.push(rel);
                }
            }
            for default_entry in ["src/lib.rs", "src/main.rs"] {
                let rel = normalize_rel_path(&format!("{prefix}{default_entry}"));
                if file_set.contains(&rel) && !entry_files.contains(&rel) {
                    entry_files.push(rel);
                }
            }

            let mut info = RustCrateInfo {
                name: crate_ident,
                root_dir: root_dir.clone(),
                entry_files: entry_files.clone(),
                module_to_file: HashMap::new(),
                file_to_module: HashMap::new(),
                mod_children: HashMap::new(),
            };

            // Index all .rs files under this crate's directory by convention AND explicit `mod` declarations
            let src_prefix = if root_dir.is_empty() {
                "src/".to_string()
            } else {
                format!("{root_dir}/src/")
            };

            for file in file_set {
                if let Some(after_src) = file.strip_prefix(&src_prefix) {
                    if after_src == "lib.rs" || after_src == "main.rs" {
                        info.file_to_module.insert(file.clone(), String::new());
                    } else if let Some(mod_dir) = after_src.strip_suffix("/mod.rs") {
                        let mod_path = mod_dir.replace('/', "::");
                        info.module_to_file.insert(mod_path.clone(), file.clone());
                        info.file_to_module.insert(file.clone(), mod_path);
                    } else if let Some(stem) = after_src.strip_suffix(".rs")
                        && !stem.starts_with("bin/")
                    {
                        let mod_path = stem.replace('/', "::");
                        info.module_to_file.insert(mod_path.clone(), file.clone());
                        info.file_to_module.insert(file.clone(), mod_path);
                    }
                }
            }

            // Also trace explicit `mod <name>;` declarations to record parent <-> child links
            for file in file_set {
                if !file.starts_with(&prefix) || !file.ends_with(".rs") {
                    continue;
                }
                let Some(text) = contents.get(file) else {
                    continue;
                };
                for child_mod in extract_rust_mod_declarations(text) {
                    if let Some(child_file) = resolve_rust_mod_file(file, &child_mod, file_set) {
                        info.mod_children
                            .entry(file.clone())
                            .or_default()
                            .insert(child_file);
                    }
                }
            }

            crates.push(info);
        }

        // Fallback if a repo has .rs files without a Cargo.toml at root
        if crates.is_empty() && file_set.iter().any(|f| f.ends_with(".rs")) {
            let mut info = RustCrateInfo {
                name: "crate".to_string(),
                root_dir: String::new(),
                entry_files: Vec::new(),
                module_to_file: HashMap::new(),
                file_to_module: HashMap::new(),
                mod_children: HashMap::new(),
            };
            for file in file_set {
                if !file.ends_with(".rs") {
                    continue;
                }
                let rel = file.strip_prefix("src/").unwrap_or(file);
                if rel == "lib.rs" || rel == "main.rs" {
                    info.entry_files.push(file.clone());
                    info.file_to_module.insert(file.clone(), String::new());
                } else if let Some(mod_dir) = rel.strip_suffix("/mod.rs") {
                    let mod_path = mod_dir.replace('/', "::");
                    info.module_to_file.insert(mod_path.clone(), file.clone());
                    info.file_to_module.insert(file.clone(), mod_path);
                } else if let Some(stem) = rel.strip_suffix(".rs") {
                    let mod_path = stem.replace('/', "::");
                    info.module_to_file.insert(mod_path.clone(), file.clone());
                    info.file_to_module.insert(file.clone(), mod_path);
                }
                if let Some(text) = contents.get(file) {
                    for child_mod in extract_rust_mod_declarations(text) {
                        if let Some(child_file) = resolve_rust_mod_file(file, &child_mod, file_set)
                        {
                            info.mod_children
                                .entry(file.clone())
                                .or_default()
                                .insert(child_file);
                        }
                    }
                }
            }
            crates.push(info);
        }

        // Sort crates from deepest root_dir to shallowest so nested workspace members match first
        crates.sort_by_key(|c| std::cmp::Reverse(c.root_dir.len()));
        let mut by_name = HashMap::new();
        for (idx, c) in crates.iter().enumerate() {
            by_name.insert(c.name.clone(), idx);
        }

        Self { crates, by_name }
    }

    fn crate_for_file(&self, file_path: &str) -> Option<&RustCrateInfo> {
        self.crates.iter().find(|c| {
            c.root_dir.is_empty()
                || file_path.starts_with(&format!("{}/", c.root_dir))
                || file_path == c.root_dir
        })
    }
}

pub(super) fn collect_cargo_tomls(root: &Path, current: &Path, out: &mut Vec<String>) {
    let Ok(read_dir) = std::fs::read_dir(current) else {
        return;
    };
    for entry in read_dir.flatten() {
        let path = entry.path();
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if ft.is_symlink() {
            continue;
        }
        if ft.is_dir() {
            if let Some(name) = path.file_name().and_then(|n| n.to_str())
                && !should_skip_dir(name)
            {
                collect_cargo_tomls(root, &path, out);
            }
        } else if ft.is_file()
            && path.file_name().and_then(|n| n.to_str()) == Some("Cargo.toml")
            && let Ok(rel) = path.strip_prefix(root)
        {
            out.push(normalize_path_buf(rel));
        }
    }
}

fn extract_rust_mod_declarations(content: &str) -> Vec<String> {
    let clean = strip_c_style_comments(content);
    let mut mods = Vec::new();
    for line in clean.lines() {
        let trimmed = line.trim();
        let after_vis = strip_rust_visibility(trimmed);
        if let Some(rest) = after_vis.strip_prefix("mod ") {
            let rest = rest.trim();
            if let Some(name) = rest.strip_suffix(';') {
                let ident = name.trim();
                if is_valid_ident(ident) {
                    mods.push(ident.to_string());
                }
            }
        }
    }
    mods
}

fn resolve_rust_mod_file(
    parent_file: &str,
    mod_name: &str,
    file_set: &BTreeSet<String>,
) -> Option<String> {
    let parent_path = Path::new(parent_file);
    let parent_dir = parent_path.parent().unwrap_or_else(|| Path::new(""));
    let stem = parent_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("");

    let search_dirs = if matches!(stem, "lib" | "main" | "mod") {
        vec![parent_dir.to_path_buf()]
    } else {
        vec![parent_dir.join(stem), parent_dir.to_path_buf()]
    };

    for dir in search_dirs {
        for candidate in [
            dir.join(format!("{mod_name}.rs")),
            dir.join(mod_name).join("mod.rs"),
        ] {
            let norm = normalize_path_buf(&candidate);
            if file_set.contains(&norm) {
                return Some(norm);
            }
        }
    }
    None
}

pub(super) fn resolve_rust_dependencies(
    file_path: &str,
    content: &str,
    _file_set: &BTreeSet<String>,
    workspace: &RustWorkspaceIndex,
) -> BTreeSet<String> {
    let mut deps = BTreeSet::new();
    let Some(krate) = workspace.crate_for_file(file_path) else {
        return deps;
    };

    // 1. Direct `mod foo;` children are dependencies of the declaring parent module
    if let Some(children) = krate.mod_children.get(file_path) {
        deps.extend(children.iter().cloned());
    }

    let current_mod = krate
        .file_to_module
        .get(file_path)
        .cloned()
        .unwrap_or_default();

    // 2. Extract all `use ...;` trees and inline `crate::...` / `super::...` paths
    let clean = strip_c_style_comments(content);
    let use_paths = extract_rust_paths(&clean);

    for raw_path in use_paths {
        let segments: Vec<&str> = raw_path
            .split("::")
            .map(str::trim)
            .filter(|s| !s.is_empty() && *s != "*" && *s != "self")
            .collect();
        if segments.is_empty() {
            continue;
        }

        match segments[0] {
            "crate" => {
                let before = deps.len();
                resolve_module_prefix_in_crate(krate, &segments[1..], &mut deps);
                if deps.len() == before {
                    deps.extend(krate.entry_files.iter().cloned());
                }
            }
            "super" => {
                let mut mod_parts: Vec<&str> = if current_mod.is_empty() {
                    Vec::new()
                } else {
                    current_mod.split("::").collect()
                };
                let mut idx = 0;
                while idx < segments.len() && segments[idx] == "super" {
                    mod_parts.pop();
                    idx += 1;
                }
                // If `super::...` refers to the parent module itself (e.g. `use super::*;`)
                let parent_mod_str = mod_parts.join("::");
                if parent_mod_str.is_empty() {
                    for entry in &krate.entry_files {
                        deps.insert(entry.clone());
                    }
                } else if let Some(parent_file) = krate.module_to_file.get(&parent_mod_str) {
                    deps.insert(parent_file.clone());
                }
                let mut combined: Vec<&str> = mod_parts;
                combined.extend_from_slice(&segments[idx..]);
                resolve_module_prefix_in_crate(krate, &combined, &mut deps);
            }
            first => {
                // Check if `first` is a sibling/child module or top-level module in the same crate
                if !current_mod.is_empty() {
                    let mut scoped: Vec<&str> = current_mod.split("::").collect();
                    scoped.extend_from_slice(&segments);
                    resolve_module_prefix_in_crate(krate, &scoped, &mut deps);
                }
                resolve_module_prefix_in_crate(krate, &segments, &mut deps);

                // Check if `first` is another workspace crate!
                if let Some(&other_idx) = workspace.by_name.get(first) {
                    let other_crate = &workspace.crates[other_idx];
                    for entry in &other_crate.entry_files {
                        deps.insert(entry.clone());
                    }
                    resolve_module_prefix_in_crate(other_crate, &segments[1..], &mut deps);
                }
            }
        }
    }

    deps
}

fn resolve_module_prefix_in_crate(
    krate: &RustCrateInfo,
    segments: &[&str],
    deps: &mut BTreeSet<String>,
) {
    if segments.is_empty() {
        for entry in &krate.entry_files {
            deps.insert(entry.clone());
        }
        return;
    }
    // Try progressively shorter prefixes of `segments` to match a module file
    // e.g. `crate::orchestrator::state::Assignment` matches `orchestrator::state` and `orchestrator`.
    for len in (1..=segments.len()).rev() {
        let candidate_mod = segments[..len].join("::");
        if let Some(target_file) = krate.module_to_file.get(&candidate_mod) {
            deps.insert(target_file.clone());
        }
    }
}

fn extract_rust_paths(clean_content: &str) -> Vec<String> {
    let mut paths = Vec::new();

    // 1. Parse `use ...;` statements (including multi-line and nested braces `use a::{b, c::d};`)
    let mut rest = clean_content;
    while let Some(pos) = find_keyword_use(rest) {
        let after_use = &rest[pos + 3..];
        if let Some(semi) = after_use.find(';') {
            let expr = after_use[..semi].trim();
            let expr = expr.strip_prefix("::").unwrap_or(expr);
            expand_use_tree("", expr, &mut paths);
            rest = &after_use[semi + 1..];
        } else {
            break;
        }
    }

    // 2. Scan inline qualified paths like `crate::foo::bar` or `super::foo::bar`
    for prefix in ["crate::", "super::"] {
        let mut scan = clean_content;
        while let Some(pos) = scan.find(prefix) {
            let valid_start = pos == 0
                || (!scan.as_bytes()[pos - 1].is_ascii_alphanumeric()
                    && scan.as_bytes()[pos - 1] != b'_');
            let tail = &scan[pos..];
            let mut end = 0;
            let bytes = tail.as_bytes();
            while end < bytes.len()
                && (bytes[end].is_ascii_alphanumeric()
                    || bytes[end] == b'_'
                    || (bytes[end] == b':' && end + 1 < bytes.len() && bytes[end + 1] == b':'))
            {
                if bytes[end] == b':' {
                    end += 2;
                } else {
                    end += 1;
                }
            }
            if valid_start && end > prefix.len() {
                paths.push(tail[..end].to_string());
            }
            scan = &tail[prefix.len()..];
        }
    }

    paths
}

fn find_keyword_use(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i + 4 <= bytes.len() {
        if &bytes[i..i + 3] == b"use"
            && (bytes[i + 3].is_ascii_whitespace())
            && (i == 0 || (!bytes[i - 1].is_ascii_alphanumeric() && bytes[i - 1] != b'_'))
        {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn expand_use_tree(prefix: &str, tree: &str, out: &mut Vec<String>) {
    let tree = tree.trim();
    if tree.is_empty() {
        return;
    }
    if let Some(open_brace) = tree.find('{')
        && let Some(close_brace) = tree.rfind('}')
        && close_brace > open_brace
    {
        let stem = tree[..open_brace].trim().trim_end_matches("::");
        let combined_prefix = match (prefix.is_empty(), stem.is_empty()) {
            (true, true) => String::new(),
            (true, false) => stem.to_string(),
            (false, true) => prefix.to_string(),
            (false, false) => format!("{prefix}::{stem}"),
        };
        let inner = &tree[open_brace + 1..close_brace];
        for item in split_top_level_commas(inner) {
            expand_use_tree(&combined_prefix, item, out);
        }
        return;
    }

    let item = tree
        .split(" as ")
        .next()
        .unwrap_or(tree)
        .trim()
        .trim_end_matches("::");
    let full = if prefix.is_empty() {
        item.to_string()
    } else if item.is_empty() || item == "self" {
        prefix.to_string()
    } else {
        format!("{prefix}::{item}")
    };
    if !full.is_empty() {
        out.push(full);
    }
}

fn split_top_level_commas(s: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    for (i, c) in s.char_indices() {
        match c {
            '{' => depth += 1,
            '}' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                parts.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    if start <= s.len() {
        parts.push(&s[start..]);
    }
    parts
}
