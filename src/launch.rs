//! Declarative worker groups. Reuse the worker CLI schema instead of maintaining
//! a second set of defaults, value parsers and executable arguments.
use anyhow::{Context, Result, bail, ensure};
use clap::{Args, CommandFactory, Parser};
use serde::Deserialize;
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::process::Command;
use toml::{Table, Value};

#[derive(Args)]
pub struct Options {
    #[arg(long, default_value = "buddies.toml")]
    config: PathBuf,
    /// Validate and print the resolved worker arguments without starting anything.
    #[arg(long)]
    check: bool,
    /// Resume every worker's saved state (requires an existing session).
    #[arg(long)]
    resume: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    room: String,
    url: Option<String>,
    #[serde(default)]
    defaults: Table,
    specialists: Vec<Table>,
}

#[derive(Parser)]
struct WorkerCli {
    #[command(flatten)]
    options: crate::worker::Options,
}

struct Worker {
    name: String,
    args: Vec<String>,
}

fn absolute(base: &Path, raw: &str) -> Result<PathBuf> {
    ensure!(!raw.is_empty(), "path must not be empty");
    let path = if raw == "~" || raw.starts_with("~/") {
        dirs::home_dir()
            .context("cannot resolve home directory")?
            .join(raw.strip_prefix("~/").unwrap_or(""))
    } else {
        base.join(raw)
    };
    Ok(path)
}

fn arguments(table: &Table) -> Result<Vec<String>> {
    let schema = WorkerCli::command();
    let mut result = Vec::new();
    for (key, value) in table {
        let arg = schema
            .get_arguments()
            .find(|arg| arg.get_long() == Some(key))
            .with_context(|| {
                format!("unknown worker option '{key}' (use names from buddies worker --help)")
            })?;
        if matches!(arg.get_action(), clap::ArgAction::SetTrue) {
            let enabled = value
                .as_bool()
                .with_context(|| format!("'{key}' must be a boolean"))?;
            if enabled {
                result.push(format!("--{key}"));
            }
            continue;
        }
        let values = match value {
            Value::Array(values) => {
                ensure!(
                    matches!(arg.get_action(), clap::ArgAction::Append),
                    "'{key}' does not accept an array"
                );
                values.as_slice()
            }
            _ => std::slice::from_ref(value),
        };
        for value in values {
            let value = match value {
                Value::String(s) => s.clone(),
                Value::Integer(i) => i.to_string(),
                _ => {
                    bail!("'{key}' requires a string or integer (or array for repeatable options)")
                }
            };
            // Equals syntax preserves values beginning with '-' as values.
            result.push(format!("--{key}={value}"));
        }
    }
    Ok(result)
}

fn load(path: &Path, resume: bool) -> Result<(PathBuf, Vec<Worker>)> {
    let path = path.canonicalize().context("open worker configuration")?;
    let base = path
        .parent()
        .context("configuration has no parent directory")?
        .to_path_buf();
    let config: Config = toml::from_str(&std::fs::read_to_string(&path)?)
        .with_context(|| format!("parse {}", path.display()))?;
    ensure!(!config.room.trim().is_empty(), "room must not be empty");
    ensure!(
        !config.specialists.is_empty(),
        "specify at least one [[specialists]] entry"
    );
    ensure!(
        !config.defaults.contains_key("name"),
        "name belongs in each [[specialists]] entry"
    );
    arguments(&config.defaults).context("invalid [defaults]")?;
    let mut identities = HashSet::new();
    let mut state_dirs = HashSet::new();
    let mut workers = Vec::new();
    for (index, specialist) in config.specialists.into_iter().enumerate() {
        let build = || -> Result<Worker> {
            let mut table = config.defaults.clone();
            table.insert("room".into(), Value::String(config.room.clone()));
            if let Some(url) = &config.url {
                table.insert("url".into(), Value::String(url.clone()));
            }
            table.extend(specialist);
            if resume {
                table.insert("resume".into(), Value::Boolean(true));
            }
            table
                .entry("cwd")
                .or_insert(Value::String(base.to_string_lossy().into_owned()));
            for field in ["cwd", "data-dir", "codex-bin", "claude-bin"] {
                if let Some(value) = table.get_mut(field) {
                    let raw = value
                        .as_str()
                        .with_context(|| format!("'{field}' must be a string"))?;
                    // A bare executable is resolved by PATH in the worker terminal.
                    if field.ends_with("-bin") && !raw.contains('/') {
                        continue;
                    }
                    *value = Value::String(absolute(&base, raw)?.to_string_lossy().into_owned());
                }
            }
            if let Some(resources) = table.get_mut("resource") {
                let resources = match resources {
                    Value::Array(values) => values.as_mut_slice(),
                    value => std::slice::from_mut(value),
                };
                for value in resources {
                    let raw = value.as_str().context("resource must be a string")?;
                    let (label, raw_path) = raw
                        .split_once('=')
                        .map_or((None, raw), |(l, r)| (Some(l), r));
                    let path = absolute(&base, raw_path.trim())?;
                    *value = Value::String(match label {
                        Some(label) => format!("{label}={}", path.display()),
                        None => path.display().to_string(),
                    });
                }
            }
            let args = arguments(&table)?;
            let parsed = WorkerCli::try_parse_from(
                std::iter::once("worker".to_owned()).chain(args.clone()),
            )?
            .options;
            let name = parsed
                .name
                .as_deref()
                .context("each specialist requires name")?;
            crate::conversation::validate_name(name)?;
            ensure!(!parsed.room.trim().is_empty(), "room must not be empty");
            let url = reqwest::Url::parse(&parsed.url).context("invalid worker URL")?;
            ensure!(
                matches!(url.scheme(), "http" | "https") && url.host_str().is_some(),
                "worker URL must be HTTP(S)"
            );
            let endpoint = parsed.url.trim_end_matches('/').trim_end_matches("/mcp");
            ensure!(
                identities.insert((endpoint.to_owned(), parsed.room.clone(), name.to_owned())),
                "duplicate worker '{name}' in room '{}'",
                parsed.room
            );
            ensure!(
                parsed.cwd.as_ref().is_some_and(|p| p.is_dir()),
                "cwd must be an existing directory"
            );
            if let Some(dir) = &parsed.data_dir {
                ensure!(
                    state_dirs.insert(dir.clone()),
                    "workers must not share data-dir"
                );
            }
            if parsed.agent == "claude" {
                crate::claude_worker::validate_options(&parsed)?;
            } else {
                ensure!(
                    parsed.linear_team.is_empty() && parsed.linear_access == "auto",
                    "live Linear options require agent = 'claude'"
                );
                ensure!(
                    name != "orchestrator" || parsed.orchestrator,
                    "name 'orchestrator' requires orchestrator = true"
                );
                let mut resource_ids = HashSet::new();
                for raw in &parsed.resources {
                    let spec = crate::orchestrator::connector::ResourceSpec::parse(raw)?;
                    ensure!(spec.path.is_dir(), "resource must be a directory");
                    ensure!(
                        resource_ids.insert(spec.descriptor.id),
                        "duplicate resource id for '{name}'"
                    );
                }
            }
            Ok(Worker {
                name: name.to_owned(),
                args,
            })
        };
        workers.push(build().with_context(|| format!("specialists[{}]", index + 1))?);
    }
    Ok((base, workers))
}

struct Tmux {
    socket: String,
}
impl Tmux {
    async fn call(&self, args: &[&str]) -> Result<String> {
        let output = Command::new("tmux")
            .args(["-L", &self.socket, "-f", "/dev/null"])
            .args(args)
            .output()
            .await
            .context("start tmux; install tmux to use buddies up")?;
        ensure!(
            output.status.success(),
            "tmux {}: {}",
            args[0],
            String::from_utf8_lossy(&output.stderr).trim()
        );
        Ok(String::from_utf8(output.stdout)?.trim().to_owned())
    }
    async fn shutdown(&self, panes: &[String]) {
        for pane in panes {
            let _ = self.call(&["send-keys", "-t", pane, "C-c"]).await;
        }
        let _ = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let Ok(states) = self.call(&["list-panes", "-a", "-F", "#{pane_dead}"]).await
                else {
                    break;
                };
                if states.lines().all(|state| state == "1") {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await;
    }
}
impl Drop for Tmux {
    fn drop(&mut self) {
        // Private socket: never kill the user's other tmux sessions.
        let _ = std::process::Command::new("tmux")
            .args(["-L", &self.socket, "kill-server"])
            .output();
    }
}

pub async fn run(options: Options) -> Result<()> {
    let (base, workers) = load(&options.config, options.resume)?;
    if options.check {
        for worker in &workers {
            println!("{}: {}", worker.name, serde_json::to_string(&worker.args)?);
        }
        println!(
            "Configuration valid ({} workers); no workers started.",
            workers.len()
        );
        return Ok(());
    }
    let tmux = Tmux {
        socket: format!("buddies-{}", uuid::Uuid::new_v4().simple()),
    };
    // Install signal handling before any child is launched.
    let signal = crate::shutdown_signal();
    tokio::pin!(signal);
    let mut panes = Vec::new();
    let result = async {
        let bootstrap = tmux
            .call(&[
                "new-session",
                "-d",
                "-s",
                "specialists",
                "-P",
                "-F",
                "#{window_id}",
                "sleep",
                "2147483647",
            ])
            .await?;
        tmux.call(&["set-option", "-g", "remain-on-exit", "on"])
            .await?;
        let binary = std::env::current_exe()?;
        for worker in &workers {
            let mut args = vec![
                "new-window",
                "-d",
                "-t",
                "specialists:",
                "-P",
                "-F",
                "#{pane_id}",
                "-n",
                &worker.name,
                "-c",
                base.to_str().context("non-UTF8 config path")?,
                binary.to_str().context("non-UTF8 executable path")?,
                "worker",
            ];
            args.extend(worker.args.iter().map(String::as_str));
            panes.push(tmux.call(&args).await?);
        }
        tmux.call(&["kill-window", "-t", &bootstrap]).await?;
        eprintln!(
            "Started {} worker processes. Attach for output and Claude consent:\n  tmux -L {} attach -t specialists\nCtrl-C here stops the group; detaching tmux leaves it running. Process launch does not imply agent readiness.",
            workers.len(),
            tmux.socket
        );
        loop {
            let states = tmux
                .call(&[
                    "list-panes",
                    "-a",
                    "-F",
                    "#{pane_id} #{pane_dead} #{pane_dead_status}",
                ])
                .await?;
            for line in states.lines() {
                let fields: Vec<_> = line.split_whitespace().collect();
                if fields.get(1) == Some(&"1") {
                    let index = panes
                        .iter()
                        .position(|pane| Some(pane.as_str()) == fields.first().copied())
                        .context("unknown worker pane")?;
                    let output = tmux
                        .call(&["capture-pane", "-p", "-t", &panes[index], "-S", "-30"])
                        .await
                        .unwrap_or_default();
                    bail!(
                        "worker '{}' exited (status {}); stopping group. Last output:\n{}",
                        workers[index].name,
                        fields.get(2).unwrap_or(&"unknown"),
                        output
                    );
                }
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    };
    let result = tokio::select! {
        result = result => result,
        _ = &mut signal => Ok(()),
    };
    tmux.shutdown(&panes).await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(text: &str, resume: bool) -> Result<(PathBuf, Vec<Worker>)> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("buddies.toml");
        std::fs::write(&path, text)?;
        load(&path, resume)
    }

    #[test]
    fn merges_defaults_and_preserves_values_without_shell_expansion() {
        let (base, workers) = config(
            r#"
room = 'project'
[defaults]
model = 'default'
check = ['echo "hello"; $(touch /tmp/never)']
resume = true
[[specialists]]
name = 'backend'
model = 'override'
resume = false
resource = ['fs:docs=.']
[[specialists]]
name = 'frontend'
"#,
            false,
        )
        .unwrap();
        assert_eq!(workers.len(), 2);
        assert!(workers[0].args.contains(&"--model=override".into()));
        assert!(!workers[0].args.contains(&"--resume".into()));
        assert!(workers[1].args.contains(&"--resume".into()));
        assert!(
            workers[0]
                .args
                .contains(&"--check=echo \"hello\"; $(touch /tmp/never)".into())
        );
        assert!(
            workers[0]
                .args
                .contains(&format!("--resource=fs:docs={}/.", base.display()))
        );
    }

    #[test]
    fn rejects_unknown_fields_and_wrong_types() {
        for text in [
            "room='r'\nspecialists=[]\nunknown=true",
            "room='r'\n[[specialists]]\nname='a'\nresuem=true",
            "room='r'\n[[specialists]]\nname='a'\nresuem=false",
            "room='r'\n[[specialists]]\nname='a'\nresume='yes'",
            "room='r'\n[[specialists]]\nname='a'\nmodel=['a','b']",
            "room='r'\n[[specialists]]\nname='a'\nsandbox='unsafe'",
        ] {
            assert!(config(text, false).is_err(), "accepted {text}");
        }
    }

    #[test]
    fn validates_entire_group_and_duplicate_identities() {
        for text in [
            "room='r'\nspecialists=[]",
            "room='r'\n[[specialists]]\nname='a'\n[[specialists]]\nname='a'",
            "room='r'\n[[specialists]]\nname='a'\n[[specialists]]\nagent='codex'",
            "room='r'\n[[specialists]]\nname='a'\nresource=['fs:a=missing']",
            "room='r'\n[[specialists]]\nname='a'\nagent='claude'",
            "room='r'\n[[specialists]]\nname='a'\nlinear-team=['ENG']",
            "room='r'\n[[specialists]]\nname='a'\ndata-dir='state'\n[[specialists]]\nname='b'\ndata-dir='state'",
        ] {
            assert!(config(text, false).is_err(), "accepted {text}");
        }
    }

    #[test]
    fn mixed_agents_and_resume_override() {
        let (_, workers) = config(
            r#"
room = 'r'
[[specialists]]
name = 'backend'
resume = false
[[specialists]]
name = 'linear'
agent = 'claude'
linear-team = ['ENG', 'ZYP']
linear-access = 'mcp'
"#,
            true,
        )
        .unwrap();
        assert!(workers.iter().all(|w| w.args.contains(&"--resume".into())));
        assert!(workers[1].args.contains(&"--linear-team=ZYP".into()));
    }
}
