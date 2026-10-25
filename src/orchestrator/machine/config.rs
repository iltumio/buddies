//! Machine-level orchestrator configuration (TOML and JSON) and CLI option resolution.
use super::{
    Options,
    state::{Config, ResourceConfig, Specialist, text, validate_path},
};
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use serde_json::{Map, Value};
use std::{
    collections::{BTreeMap, HashSet},
    path::{Path, PathBuf},
};

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default)]
    orchestrator: Option<RawOrchestratorSection>,
    #[serde(default)]
    room: Option<String>,
    #[serde(default)]
    repo: Option<PathBuf>,
    #[serde(default)]
    goal: Option<String>,
    #[serde(default, alias = "allow_paths")]
    authorized_paths: Vec<String>,
    #[serde(default)]
    checks: Vec<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    codex_bin: Option<PathBuf>,
    #[serde(default)]
    state_dir: Option<PathBuf>,
    #[serde(default)]
    max_active_specialists: Option<usize>,
    #[serde(default)]
    max_model_calls: Option<u64>,
    #[serde(default)]
    max_output_bytes: Option<u64>,
    #[serde(default)]
    max_elapsed_secs: Option<u64>,
    #[serde(default)]
    max_depth: Option<usize>,
    #[serde(default)]
    check_timeout_secs: Option<u64>,
    #[serde(default)]
    debounce_ms: Option<u64>,
    #[serde(default)]
    resources: Option<RawResources>,
    #[serde(default)]
    specialists: Vec<RawSpecialist>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawOrchestratorSection {
    #[serde(default)]
    room: Option<String>,
    #[serde(default)]
    repo: Option<PathBuf>,
    #[serde(default)]
    goal: Option<String>,
    #[serde(default, alias = "allow_paths")]
    authorized_paths: Vec<String>,
    #[serde(default)]
    checks: Vec<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    codex_bin: Option<PathBuf>,
    #[serde(default)]
    state_dir: Option<PathBuf>,
    #[serde(default)]
    max_active_specialists: Option<usize>,
    #[serde(default)]
    max_model_calls: Option<u64>,
    #[serde(default)]
    max_output_bytes: Option<u64>,
    #[serde(default)]
    max_elapsed_secs: Option<u64>,
    #[serde(default)]
    max_depth: Option<usize>,
    #[serde(default)]
    check_timeout_secs: Option<u64>,
    #[serde(default)]
    debounce_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawResources {
    List(Vec<RawResource>),
    Map(BTreeMap<String, RawResourceEntry>),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawResource {
    name: String,
    #[serde(alias = "path")]
    repo: PathBuf,
    #[serde(default)]
    description: Option<String>,
    #[serde(default, alias = "allow_paths")]
    authorized_paths: Vec<String>,
    #[serde(default)]
    checks: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawResourceEntry {
    #[serde(default)]
    name: Option<String>,
    #[serde(alias = "path")]
    repo: PathBuf,
    #[serde(default)]
    description: Option<String>,
    #[serde(default, alias = "allow_paths")]
    authorized_paths: Vec<String>,
    #[serde(default)]
    checks: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawSpecialist {
    pub name: String,
    pub role: String,
    #[serde(default)]
    pub resources: Vec<String>,
    #[serde(default, alias = "paths")]
    pub allowed_paths: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Resolved {
    pub room: String,
    pub url: String,
    pub codex_bin: PathBuf,
    pub state_dir: Option<PathBuf>,
    pub debounce_ms: u64,
    pub resume: bool,
    pub once: bool,
    pub service_mode: bool,
    pub from_config_file: bool,
    pub config: Config,
    pub initial_specialists: BTreeMap<String, Specialist>,
}

impl Resolved {
    pub fn from_options(options: Options) -> Result<Self> {
        let from_config_file = options.config.is_some();
        let raw = if let Some(path) = &options.config {
            let content = std::fs::read_to_string(path)
                .with_context(|| format!("read orchestrator config {}", path.display()))?;
            parse_config_str(&content)
                .with_context(|| format!("parse orchestrator config {}", path.display()))?
        } else {
            RawConfig::default()
        };
        Self::resolve(options, raw, from_config_file)
    }

    fn resolve(options: Options, raw: RawConfig, from_config_file: bool) -> Result<Self> {
        let sec = raw.orchestrator.unwrap_or_default();
        let room = options
            .room
            .or(raw.room)
            .or(sec.room)
            .context("room is required (pass --room or set room in --config)")?;
        ensure!(!room.trim().is_empty() && room.len() <= 256, "invalid room");

        let url = options
            .url
            .or(raw.url)
            .or(sec.url)
            .unwrap_or_else(|| "http://127.0.0.1:8080".into());
        let codex_bin = options
            .codex_bin
            .or(raw.codex_bin)
            .or(sec.codex_bin)
            .unwrap_or_else(|| PathBuf::from("codex"));
        let model = options.model.or(raw.model).or(sec.model);
        let state_dir = options.state_dir.or(raw.state_dir).or(sec.state_dir);
        let goal = options.goal.or(raw.goal).or(sec.goal);
        if let Some(g) = &goal {
            text(g)?;
        }

        let max_active = options
            .max_active_specialists
            .or(raw.max_active_specialists)
            .or(sec.max_active_specialists)
            .unwrap_or(2);
        let max_calls = options
            .max_model_calls
            .or(raw.max_model_calls)
            .or(sec.max_model_calls)
            .unwrap_or(64);
        let max_output_bytes = options
            .max_output_bytes
            .or(raw.max_output_bytes)
            .or(sec.max_output_bytes)
            .unwrap_or(1_048_576);
        let max_elapsed_secs = options
            .max_elapsed_secs
            .or(raw.max_elapsed_secs)
            .or(sec.max_elapsed_secs)
            .unwrap_or(3600);
        let max_depth = options
            .max_depth
            .or(raw.max_depth)
            .or(sec.max_depth)
            .unwrap_or(8);
        let check_timeout_secs = options
            .check_timeout_secs
            .or(raw.check_timeout_secs)
            .or(sec.check_timeout_secs)
            .unwrap_or(120);
        let debounce_ms = options
            .debounce_ms
            .or(raw.debounce_ms)
            .or(sec.debounce_ms)
            .unwrap_or(1000);

        ensure!(
            (1..=16).contains(&max_active),
            "max active specialists must be 1..16"
        );
        ensure!(
            max_calls > 0
                && max_calls <= 10000
                && max_output_bytes > 0
                && max_output_bytes <= 64 * 1024 * 1024
                && max_elapsed_secs > 0
                && max_elapsed_secs <= 7 * 86400
                && check_timeout_secs > 0
                && check_timeout_secs <= max_elapsed_secs
                && max_depth <= 32
                && (50..=10000).contains(&debounce_ms),
            "invalid runtime budget/debounce"
        );

        let mut resources = BTreeMap::new();
        if let Some(raw_resources) = raw.resources {
            let list: Vec<RawResource> = match raw_resources {
                RawResources::List(list) => list,
                RawResources::Map(map) => map
                    .into_iter()
                    .map(|(k, v)| RawResource {
                        name: v.name.unwrap_or(k),
                        repo: v.repo,
                        description: v.description,
                        authorized_paths: v.authorized_paths,
                        checks: v.checks,
                    })
                    .collect(),
            };
            ensure!(
                !list.is_empty() && list.len() <= 16,
                "resources catalog must contain 1..=16 resources"
            );
            let mut seen_repos = HashSet::new();
            for item in list {
                crate::conversation::validate_name(&item.name)
                    .with_context(|| format!("invalid resource name '{}'", item.name))?;
                ensure!(
                    !resources.contains_key(&item.name),
                    "duplicate resource name '{}'",
                    item.name
                );
                let repo = canonicalize_repo(&item.repo)?;
                ensure!(
                    seen_repos.insert(repo.clone()),
                    "duplicate repository path in resources: {}",
                    repo.display()
                );
                if let Some(desc) = &item.description {
                    text(desc)?;
                }
                validate_resource_scope(&item.authorized_paths, &item.checks, true)?;
                resources.insert(
                    item.name.clone(),
                    ResourceConfig {
                        name: item.name,
                        repo,
                        description: item.description,
                        authorized_paths: item.authorized_paths,
                        checks: item.checks,
                    },
                );
            }
        }

        let cli_repo = options.repo.or(raw.repo).or(sec.repo);
        let cli_paths = if !options.authorized_paths.is_empty() {
            options.authorized_paths
        } else if !raw.authorized_paths.is_empty() {
            raw.authorized_paths
        } else {
            sec.authorized_paths
        };
        let cli_checks = if !options.checks.is_empty() {
            options.checks
        } else if !raw.checks.is_empty() {
            raw.checks
        } else {
            sec.checks
        };

        if resources.is_empty() {
            let repo_path = cli_repo.context(
                "repository is required (pass --repo or define [[resources]] in --config)",
            )?;
            let repo = canonicalize_repo(&repo_path)?;
            if !from_config_file {
                ensure!(
                    !cli_paths.is_empty(),
                    "at least one --allow-path is required when running without --config"
                );
                ensure!(
                    !cli_checks.is_empty(),
                    "at least one --check is required when running without --config"
                );
            }
            validate_resource_scope(&cli_paths, &cli_checks, from_config_file)?;
            if from_config_file {
                resources.insert(
                    "default".into(),
                    ResourceConfig {
                        name: "default".into(),
                        repo: repo.clone(),
                        description: None,
                        authorized_paths: cli_paths.clone(),
                        checks: cli_checks.clone(),
                    },
                );
            }
            let service_mode = goal.is_none();
            let config = Config {
                room: room.clone(),
                repo,
                resources,
                model,
                goal: goal.unwrap_or_default(),
                authorized_paths: cli_paths,
                checks: cli_checks,
                max_active,
                max_calls,
                max_output_bytes,
                max_elapsed_secs,
                max_depth,
                check_timeout_secs,
            };
            let initial_specialists =
                resolve_specialists(&raw.specialists, &config, from_config_file, service_mode)?;
            return Ok(Self {
                room,
                url,
                codex_bin,
                state_dir,
                debounce_ms,
                resume: options.resume,
                once: options.once,
                service_mode,
                from_config_file,
                config,
                initial_specialists,
            });
        }

        let primary = resources
            .get("default")
            .or_else(|| resources.values().next())
            .expect("non-empty resources")
            .clone();
        let service_mode = goal.is_none();
        let config = Config {
            room: room.clone(),
            repo: primary.repo,
            resources,
            model,
            goal: goal.unwrap_or_default(),
            authorized_paths: primary.authorized_paths,
            checks: primary.checks,
            max_active,
            max_calls,
            max_output_bytes,
            max_elapsed_secs,
            max_depth,
            check_timeout_secs,
        };
        let initial_specialists =
            resolve_specialists(&raw.specialists, &config, from_config_file, service_mode)?;
        Ok(Self {
            room,
            url,
            codex_bin,
            state_dir,
            debounce_ms,
            resume: options.resume,
            once: options.once,
            service_mode,
            from_config_file,
            config,
            initial_specialists,
        })
    }
}

fn canonicalize_repo(path: &Path) -> Result<PathBuf> {
    path.canonicalize()
        .with_context(|| format!("repository does not exist: {}", path.display()))
}

fn validate_resource_scope(
    authorized_paths: &[String],
    checks: &[String],
    allow_wildcard: bool,
) -> Result<()> {
    ensure!(
        authorized_paths.len() <= 32 && checks.len() <= 8,
        "too many paths/checks"
    );
    for path in authorized_paths {
        if allow_wildcard && (path == "." || path == "*") {
            continue;
        }
        validate_path(path)?;
    }
    for check in checks {
        text(check)?;
    }
    Ok(())
}

fn resolve_specialists(
    raw: &[RawSpecialist],
    config: &Config,
    from_config_file: bool,
    service_mode: bool,
) -> Result<BTreeMap<String, Specialist>> {
    ensure!(raw.len() <= 16, "too many preconfigured specialists");
    let mut map = BTreeMap::new();
    for item in raw {
        crate::conversation::validate_name(&item.name)?;
        ensure!(
            item.name != super::state::COORDINATOR,
            "orchestrator cannot be a specialist"
        );
        ensure!(
            !map.contains_key(&item.name),
            "duplicate specialist '{}'",
            item.name
        );
        text(&item.role)?;
        let resources = if item.resources.is_empty() {
            vec![config.primary_resource_name()]
        } else {
            for r in &item.resources {
                config.resource(Some(r))?;
            }
            item.resources.clone()
        };
        ensure!(item.allowed_paths.len() <= 32, "too many specialist paths");
        for path in &item.allowed_paths {
            validate_path(path)?;
            ensure!(
                resources.iter().any(|r| {
                    config
                        .resource(Some(r))
                        .is_ok_and(|rc| rc.allows_path(path))
                }),
                "specialist path '{}' outside granted resource scope",
                path
            );
        }
        map.insert(
            item.name.clone(),
            Specialist {
                thread: None,
                cwd: None,
                role: Some(item.role.clone()),
                resources,
                allowed_paths: item.allowed_paths.clone(),
            },
        );
    }
    if map.is_empty() && !from_config_file && !service_mode {
        map.insert("specialist-a".into(), Specialist::default());
        map.insert("specialist-b".into(), Specialist::default());
    }
    Ok(map)
}

fn parse_config_str(input: &str) -> Result<RawConfig> {
    let trimmed = input.trim_start();
    if trimmed.starts_with('{') {
        return serde_json::from_str(input).context("invalid JSON config");
    }
    let value = parse_toml(input)?;
    serde_json::from_value(value).context("invalid orchestrator configuration schema")
}

/// Self-contained TOML parser producing `serde_json::Value` for orchestrator configuration.
pub fn parse_toml(input: &str) -> Result<Value> {
    let mut root = Map::new();
    let mut current_path: Vec<String> = Vec::new();
    let mut is_array_table = false;
    let mut lines = input.lines().enumerate().peekable();

    while let Some((line_idx, raw_line)) = lines.next() {
        let line_no = line_idx + 1;
        let line = strip_comment(raw_line)?.trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with("[[") && line.ends_with("]]") {
            let inner = line[2..line.len() - 2].trim();
            current_path = parse_key_path(inner, line_no)?;
            is_array_table = true;
            ensure_array_table(&mut root, &current_path, line_no)?;
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            let inner = line[1..line.len() - 1].trim();
            current_path = parse_key_path(inner, line_no)?;
            is_array_table = false;
            ensure_table(&mut root, &current_path, line_no)?;
            continue;
        }
        let (key_path, mut rhs) = split_key_value(line, line_no)?;
        while !brackets_balanced(&rhs)? {
            let (_, next_raw) = lines
                .next()
                .with_context(|| format!("line {line_no}: unclosed array or inline table"))?;
            rhs.push('\n');
            rhs.push_str(strip_comment(next_raw)?.trim());
        }
        let val = parse_value(rhs.trim(), line_no)?;
        let target = get_target_table(&mut root, &current_path, is_array_table, line_no)?;
        insert_dotted(target, &key_path, val, line_no)?;
    }

    Ok(Value::Object(root))
}

fn strip_comment(line: &str) -> Result<&str> {
    let mut in_basic = false;
    let mut in_literal = false;
    let mut escape = false;
    for (idx, ch) in line.char_indices() {
        if escape {
            escape = false;
            continue;
        }
        if in_basic {
            if ch == '\\' {
                escape = true;
            } else if ch == '"' {
                in_basic = false;
            }
        } else if in_literal {
            if ch == '\'' {
                in_literal = false;
            }
        } else if ch == '"' {
            in_basic = true;
        } else if ch == '\'' {
            in_literal = true;
        } else if ch == '#' {
            return Ok(&line[..idx]);
        }
    }
    ensure!(!in_basic && !in_literal, "unterminated string literal");
    Ok(line)
}

fn brackets_balanced(s: &str) -> Result<bool> {
    let mut depth_sq = 0i32;
    let mut depth_brace = 0i32;
    let mut in_basic = false;
    let mut in_literal = false;
    let mut escape = false;
    for ch in s.chars() {
        if escape {
            escape = false;
            continue;
        }
        if in_basic {
            if ch == '\\' {
                escape = true;
            } else if ch == '"' {
                in_basic = false;
            }
        } else if in_literal {
            if ch == '\'' {
                in_literal = false;
            }
        } else {
            match ch {
                '"' => in_basic = true,
                '\'' => in_literal = true,
                '[' => depth_sq += 1,
                ']' => depth_sq -= 1,
                '{' => depth_brace += 1,
                '}' => depth_brace -= 1,
                _ => {}
            }
            ensure!(
                depth_sq >= 0 && depth_brace >= 0,
                "unexpected closing bracket"
            );
        }
    }
    Ok(depth_sq == 0 && depth_brace == 0)
}

fn parse_key_path(s: &str, line_no: usize) -> Result<Vec<String>> {
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut quote_char = '"';
    for ch in s.chars() {
        if in_quotes {
            if ch == quote_char {
                in_quotes = false;
            } else {
                cur.push(ch);
            }
        } else if ch == '"' || ch == '\'' {
            in_quotes = true;
            quote_char = ch;
        } else if ch == '.' {
            let trimmed = cur.trim();
            ensure!(!trimmed.is_empty(), "line {line_no}: empty key segment");
            parts.push(trimmed.to_owned());
            cur.clear();
        } else {
            cur.push(ch);
        }
    }
    let trimmed = cur.trim();
    ensure!(!trimmed.is_empty(), "line {line_no}: empty key segment");
    parts.push(trimmed.to_owned());
    for part in &parts {
        ensure!(
            part.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
            "line {line_no}: invalid key '{part}'"
        );
    }
    Ok(parts)
}

fn split_key_value(line: &str, line_no: usize) -> Result<(Vec<String>, String)> {
    let mut in_quotes = false;
    let mut quote_char = '"';
    for (idx, ch) in line.char_indices() {
        if in_quotes {
            if ch == quote_char {
                in_quotes = false;
            }
        } else if ch == '"' || ch == '\'' {
            in_quotes = true;
            quote_char = ch;
        } else if ch == '=' {
            let key = parse_key_path(line[..idx].trim(), line_no)?;
            let rhs = line[idx + 1..].trim().to_owned();
            ensure!(!rhs.is_empty(), "line {line_no}: missing value after '='");
            return Ok((key, rhs));
        }
    }
    bail!("line {line_no}: expected 'key = value'")
}

fn ensure_table(root: &mut Map<String, Value>, path: &[String], line_no: usize) -> Result<()> {
    let mut cur = root;
    for seg in path {
        let entry = cur
            .entry(seg.clone())
            .or_insert_with(|| Value::Object(Map::new()));
        cur = match entry {
            Value::Object(map) => map,
            Value::Array(arr) => arr
                .last_mut()
                .and_then(Value::as_object_mut)
                .with_context(|| format!("line {line_no}: invalid table path"))?,
            _ => bail!("line {line_no}: key '{seg}' conflicts with non-table value"),
        };
    }
    Ok(())
}

fn ensure_array_table(
    root: &mut Map<String, Value>,
    path: &[String],
    line_no: usize,
) -> Result<()> {
    let (last, parents) = path
        .split_last()
        .with_context(|| format!("line {line_no}: empty table header"))?;
    let mut cur = root;
    for seg in parents {
        let entry = cur
            .entry(seg.clone())
            .or_insert_with(|| Value::Object(Map::new()));
        cur = entry
            .as_object_mut()
            .with_context(|| format!("line {line_no}: '{seg}' is not a table"))?;
    }
    let arr = cur
        .entry(last.clone())
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .with_context(|| format!("line {line_no}: '{last}' is not an array of tables"))?;
    arr.push(Value::Object(Map::new()));
    Ok(())
}

fn get_target_table<'a>(
    root: &'a mut Map<String, Value>,
    path: &[String],
    is_array_table: bool,
    line_no: usize,
) -> Result<&'a mut Map<String, Value>> {
    if path.is_empty() {
        return Ok(root);
    }
    let mut cur = root;
    for (idx, seg) in path.iter().enumerate() {
        let is_last = idx + 1 == path.len();
        let entry = cur
            .get_mut(seg)
            .with_context(|| format!("line {line_no}: missing table '{seg}'"))?;
        if is_last && is_array_table {
            cur = entry
                .as_array_mut()
                .and_then(|a| a.last_mut())
                .and_then(Value::as_object_mut)
                .with_context(|| format!("line {line_no}: invalid array table '{seg}'"))?;
        } else {
            cur = match entry {
                Value::Object(map) => map,
                Value::Array(arr) => arr
                    .last_mut()
                    .and_then(Value::as_object_mut)
                    .with_context(|| format!("line {line_no}: invalid table '{seg}'"))?,
                _ => bail!("line {line_no}: '{seg}' is not a table"),
            };
        }
    }
    Ok(cur)
}

fn insert_dotted(
    table: &mut Map<String, Value>,
    key_path: &[String],
    val: Value,
    line_no: usize,
) -> Result<()> {
    let (last, parents) = key_path.split_last().unwrap();
    let mut cur = table;
    for seg in parents {
        let entry = cur
            .entry(seg.clone())
            .or_insert_with(|| Value::Object(Map::new()));
        cur = entry
            .as_object_mut()
            .with_context(|| format!("line {line_no}: '{seg}' is not a table"))?;
    }
    ensure!(
        !cur.contains_key(last),
        "line {line_no}: duplicate key '{last}'"
    );
    cur.insert(last.clone(), val);
    Ok(())
}

fn parse_value(s: &str, line_no: usize) -> Result<Value> {
    let s = s.trim();
    if s.starts_with('"') && s.ends_with('"') && s.len() >= 2 {
        let parsed: String = serde_json::from_str(s)
            .with_context(|| format!("line {line_no}: invalid string literal"))?;
        return Ok(Value::String(parsed));
    }
    if s.starts_with('\'') && s.ends_with('\'') && s.len() >= 2 {
        return Ok(Value::String(s[1..s.len() - 1].to_owned()));
    }
    if s == "true" {
        return Ok(Value::Bool(true));
    }
    if s == "false" {
        return Ok(Value::Bool(false));
    }
    if s.starts_with('[') && s.ends_with(']') {
        let inner = s[1..s.len() - 1].trim();
        if inner.is_empty() {
            return Ok(Value::Array(Vec::new()));
        }
        let items = split_top_level(inner, ',')?;
        let mut out = Vec::new();
        for item in items {
            let trimmed = item.trim();
            if !trimmed.is_empty() {
                out.push(parse_value(trimmed, line_no)?);
            }
        }
        return Ok(Value::Array(out));
    }
    if s.starts_with('{') && s.ends_with('}') {
        let inner = s[1..s.len() - 1].trim();
        let mut map = Map::new();
        if !inner.is_empty() {
            for entry in split_top_level(inner, ',')? {
                let trimmed = entry.trim();
                if !trimmed.is_empty() {
                    let (k, v) = split_key_value(trimmed, line_no)?;
                    insert_dotted(&mut map, &k, parse_value(&v, line_no)?, line_no)?;
                }
            }
        }
        return Ok(Value::Object(map));
    }
    let clean_num = s.replace('_', "");
    if let Ok(n) = clean_num.parse::<u64>() {
        return Ok(Value::Number(n.into()));
    }
    if let Ok(n) = clean_num.parse::<i64>() {
        return Ok(Value::Number(n.into()));
    }
    bail!("line {line_no}: unsupported value '{s}'")
}

fn split_top_level(s: &str, sep: char) -> Result<Vec<String>> {
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut depth_sq = 0i32;
    let mut depth_brace = 0i32;
    let mut in_basic = false;
    let mut in_literal = false;
    let mut escape = false;
    for ch in s.chars() {
        if escape {
            cur.push(ch);
            escape = false;
            continue;
        }
        if in_basic {
            cur.push(ch);
            if ch == '\\' {
                escape = true;
            } else if ch == '"' {
                in_basic = false;
            }
        } else if in_literal {
            cur.push(ch);
            if ch == '\'' {
                in_literal = false;
            }
        } else {
            match ch {
                '"' => {
                    in_basic = true;
                    cur.push(ch);
                }
                '\'' => {
                    in_literal = true;
                    cur.push(ch);
                }
                '[' => {
                    depth_sq += 1;
                    cur.push(ch);
                }
                ']' => {
                    depth_sq -= 1;
                    cur.push(ch);
                }
                '{' => {
                    depth_brace += 1;
                    cur.push(ch);
                }
                '}' => {
                    depth_brace -= 1;
                    cur.push(ch);
                }
                c if c == sep && depth_sq == 0 && depth_brace == 0 => {
                    parts.push(std::mem::take(&mut cur));
                }
                _ => cur.push(ch),
            }
        }
    }
    parts.push(cur);
    Ok(parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_toml_machine_config_with_resources_and_specialists() {
        let dir = tempfile::tempdir().unwrap();
        let backend = dir.path().join("backend");
        let frontend = dir.path().join("frontend");
        std::fs::create_dir_all(&backend).unwrap();
        std::fs::create_dir_all(&frontend).unwrap();
        let config_path = dir.path().join("orchestrator.toml");
        std::fs::write(
            &config_path,
            format!(
                r#"
                # Machine orchestrator config
                room = "Autonomous-Room"
                max_active_specialists = 4
                max_model_calls = 128

                [[resources]]
                name = "backend"
                repo = "{}"
                description = "Backend API service"
                allow_paths = ["src", "tests"]
                checks = ["cargo test"]

                [[resources]]
                name = "frontend"
                repo = "{}"
                description = "Web client"
                allow_paths = ["."]
                checks = ["npm test"]

                [[specialists]]
                name = "api-specialist"
                role = "Maintain backend API contracts"
                resources = ["backend"]
                allowed_paths = ["src"]
                "#,
                backend.display(),
                frontend.display()
            ),
        )
        .unwrap();

        let resolved = Resolved::from_options(Options {
            config: Some(config_path),
            room: None,
            repo: None,
            goal: None,
            authorized_paths: vec![],
            checks: vec![],
            model: None,
            url: None,
            codex_bin: None,
            state_dir: None,
            max_active_specialists: None,
            max_model_calls: None,
            max_output_bytes: None,
            max_elapsed_secs: None,
            max_depth: None,
            check_timeout_secs: None,
            debounce_ms: None,
            resume: false,
            once: false,
        })
        .unwrap();

        assert!(resolved.service_mode);
        assert_eq!(resolved.room, "Autonomous-Room");
        assert_eq!(resolved.config.max_active, 4);
        assert_eq!(resolved.config.max_calls, 128);
        assert_eq!(resolved.config.resources.len(), 2);
        assert!(resolved.config.resources["backend"].allows_path("src/lib.rs"));
        assert!(!resolved.config.resources["backend"].allows_path("docs/readme.md"));
        assert!(resolved.config.resources["frontend"].allows_path("any/valid/path.ts"));
        assert_eq!(
            resolved.initial_specialists["api-specialist"].resources,
            vec!["backend"]
        );
    }
}
