use super::*;

// ============================================================================
// TypeScript / JavaScript Import Resolution
// ============================================================================

type TsPathMapping = (String, String, Vec<(String, String)>);

#[derive(Debug, Clone, Default)]
pub(super) struct TsConfigAliases {
    base_url: Option<String>,
    /// `(prefix_before_star, Vec<target_prefix_before_star>)`
    path_mappings: Vec<TsPathMapping>,
}

impl TsConfigAliases {
    pub(super) fn load(repo_root: &Path) -> Self {
        for candidate in ["tsconfig.json", "tsconfig.base.json", "jsconfig.json"] {
            let path = repo_root.join(candidate);
            if let Ok(raw) = std::fs::read_to_string(&path)
                && let Some(parsed) = Self::parse_json(&raw)
            {
                return parsed;
            }
        }
        Self::default()
    }

    fn parse_json(raw: &str) -> Option<Self> {
        let stripped = strip_jsonc_comments_and_trailing_commas(raw);
        let val: serde_json::Value = serde_json::from_str(&stripped).ok()?;
        let compiler = val.get("compilerOptions")?;
        let base_url = compiler
            .get("baseUrl")
            .and_then(|v| v.as_str())
            .map(normalize_rel_path);

        let mut path_mappings = Vec::new();
        if let Some(paths) = compiler.get("paths").and_then(|v| v.as_object()) {
            for (pattern, targets) in paths {
                let Some(arr) = targets.as_array() else {
                    continue;
                };
                let (pat_prefix, pat_suffix) = match pattern.split_once('*') {
                    Some((p, s)) => (p.to_string(), s.to_string()),
                    None => (pattern.clone(), String::new()),
                };
                let mut mapped_targets = Vec::new();
                for target in arr.iter().filter_map(|t| t.as_str()) {
                    let combined = match &base_url {
                        Some(base) if !base.is_empty() => format!("{base}/{target}"),
                        _ => target.to_string(),
                    };
                    let normalized = normalize_rel_path(&combined);
                    let (t_prefix, t_suffix) = match normalized.split_once('*') {
                        Some((p, s)) => (p.to_string(), s.to_string()),
                        None => (normalized, String::new()),
                    };
                    mapped_targets.push((t_prefix, t_suffix));
                }
                path_mappings.push((pat_prefix, pat_suffix, mapped_targets));
            }
        }

        Some(Self {
            base_url,
            path_mappings,
        })
    }

    fn expand_specifier(&self, specifier: &str) -> Vec<String> {
        let mut candidates = Vec::new();
        for (pat_prefix, pat_suffix, targets) in &self.path_mappings {
            if pat_suffix.is_empty() && specifier == pat_prefix {
                for (t_prefix, _) in targets {
                    candidates.push(t_prefix.clone());
                }
            } else if specifier.starts_with(pat_prefix) && specifier.ends_with(pat_suffix) {
                let mid_end = specifier.len() - pat_suffix.len();
                if mid_end >= pat_prefix.len() {
                    let wildcard = &specifier[pat_prefix.len()..mid_end];
                    for (t_prefix, t_suffix) in targets {
                        candidates.push(format!("{t_prefix}{wildcard}{t_suffix}"));
                    }
                }
            }
        }
        if let Some(base) = &self.base_url {
            let joined = if base.is_empty() {
                specifier.to_string()
            } else {
                format!("{base}/{specifier}")
            };
            candidates.push(normalize_rel_path(&joined));
        }
        candidates
    }
}

fn strip_jsonc_comments_and_trailing_commas(input: &str) -> String {
    let without_comments = strip_c_style_comments(input);
    let mut out = String::with_capacity(without_comments.len());
    let chars: Vec<char> = without_comments.chars().collect();
    let mut in_string = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '"' {
            let mut backslashes = 0;
            let mut k = i;
            while k > 0 && chars[k - 1] == '\\' {
                backslashes += 1;
                k -= 1;
            }
            if backslashes % 2 == 0 {
                in_string = !in_string;
            }
            out.push(c);
            i += 1;
            continue;
        }
        if !in_string && c == ',' {
            let mut j = i + 1;
            while j < chars.len() && chars[j].is_whitespace() {
                j += 1;
            }
            if j < chars.len() && (chars[j] == '}' || chars[j] == ']') {
                i += 1;
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

pub(super) fn resolve_ts_dependencies(
    importer_path: &str,
    content: &str,
    file_set: &BTreeSet<String>,
    aliases: &TsConfigAliases,
) -> BTreeSet<String> {
    let mut resolved = BTreeSet::new();
    let importer_dir = Path::new(importer_path)
        .parent()
        .unwrap_or_else(|| Path::new(""));

    for spec in extract_ts_specifiers(content) {
        if let Some(found) = resolve_ts_specifier(&spec, importer_dir, file_set, aliases) {
            resolved.insert(found);
        }
    }
    resolved
}

fn extract_ts_specifiers(content: &str) -> Vec<String> {
    #[derive(Debug)]
    enum Token {
        Word(String),
        String(String),
        Punct(char),
    }
    let clean = strip_c_style_comments(content);
    let mut tokens = Vec::new();
    let mut chars = clean.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\'' || ch == '"' || ch == '`' {
            let mut text = String::new();
            while let Some(next) = chars.next() {
                if next == ch {
                    break;
                }
                if next == '\\' {
                    if let Some(escaped) = chars.next() {
                        text.push(escaped);
                    }
                } else {
                    text.push(next);
                }
            }
            if ch != '`' {
                tokens.push(Token::String(text));
            }
        } else if ch.is_alphanumeric() || ch == '_' || ch == '$' {
            let mut word = String::from(ch);
            while chars
                .peek()
                .is_some_and(|c| c.is_alphanumeric() || *c == '_' || *c == '$')
            {
                word.push(chars.next().unwrap());
            }
            tokens.push(Token::Word(word));
        } else if !ch.is_whitespace() {
            tokens.push(Token::Punct(ch));
        }
    }
    let mut specs = Vec::new();
    for (index, token) in tokens.iter().enumerate() {
        let Token::Word(word) = token else {
            continue;
        };
        if word == "require" || word == "import" {
            match tokens.get(index + 1) {
                Some(Token::Punct('(')) => {
                    if let Some(Token::String(value)) = tokens.get(index + 2) {
                        specs.push(value.clone());
                    }
                }
                Some(Token::String(value)) if word == "import" => specs.push(value.clone()),
                _ => {}
            }
        }
        if word != "import" && word != "export" {
            continue;
        }
        for pair in tokens[index + 1..].windows(2) {
            if matches!(&pair[0], Token::Punct(';')) {
                break;
            }
            if matches!(&pair[0], Token::Word(w) if w == "from") {
                if let Token::String(value) = &pair[1] {
                    specs.push(value.clone());
                }
                break;
            }
            if matches!(&pair[0], Token::Word(w) if w == "import" || w == "export") {
                break;
            }
        }
    }
    specs
}

fn resolve_ts_specifier(
    specifier: &str,
    importer_dir: &Path,
    file_set: &BTreeSet<String>,
    aliases: &TsConfigAliases,
) -> Option<String> {
    let specifier = specifier.split('?').next().unwrap_or(specifier);
    let mut base_candidates = Vec::new();

    if specifier.starts_with("./")
        || specifier.starts_with("../")
        || specifier == "."
        || specifier == ".."
    {
        let joined = importer_dir.join(specifier);
        base_candidates.push(normalize_path_buf(&joined));
    } else if let Some(stripped) = specifier.strip_prefix('/') {
        base_candidates.push(normalize_rel_path(stripped));
    } else {
        base_candidates.extend(aliases.expand_specifier(specifier));
        // Support `@/foo` -> `src/foo` convention even without tsconfig.json
        if let Some(rest) = specifier.strip_prefix("@/") {
            base_candidates.push(normalize_rel_path(&format!("src/{rest}")));
        }
        // Also check direct repo-relative match if someone imports `src/...`
        if file_set.iter().any(|f| f.starts_with(specifier)) {
            base_candidates.push(normalize_rel_path(specifier));
        }
    }

    const EXTENSIONS: &[&str] = &[
        ".ts",
        ".tsx",
        ".mts",
        ".cts",
        ".d.ts",
        ".js",
        ".jsx",
        ".mjs",
        ".cjs",
        "/index.ts",
        "/index.tsx",
        "/index.mts",
        "/index.cts",
        "/index.d.ts",
        "/index.js",
        "/index.jsx",
    ];

    for base in base_candidates {
        if file_set.contains(&base) {
            return Some(base);
        }
        // TypeScript ESM resolution allows `import './foo.js'` to resolve to `foo.ts` or `foo.tsx`
        for (js_ext, ts_replacements) in [
            (".js", &[".ts", ".tsx", ".d.ts"][..]),
            (".jsx", &[".tsx", ".ts"][..]),
            (".mjs", &[".mts", ".d.mts"][..]),
            (".cjs", &[".cts", ".d.cts"][..]),
        ] {
            if let Some(stem) = base.strip_suffix(js_ext) {
                for ts_ext in ts_replacements {
                    let candidate = format!("{stem}{ts_ext}");
                    if file_set.contains(&candidate) {
                        return Some(candidate);
                    }
                }
            }
        }
        for ext in EXTENSIONS {
            let candidate = format!("{base}{ext}");
            if file_set.contains(&candidate) {
                return Some(candidate);
            }
        }
    }

    None
}
