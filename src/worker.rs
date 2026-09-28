//! Persistent HTTP worker and Codex App Server adapter. Never opens the server database.
use crate::worker_queue::{Action, Job, Outcome, Request, now, worker_id};
use anyhow::{Context, Result, ensure};
use clap::Args;
use redb::ReadableTable;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use uuid::Uuid;

#[derive(Args)]
pub struct Options {
    #[arg(long, default_value="codex", value_parser=["codex"])]
    pub agent: String,
    #[arg(long)]
    pub room: String,
    /// Repository in which delegated tasks may run.
    #[arg(long)]
    pub cwd: PathBuf,
    #[arg(long, env = "BUDDIES_URL", default_value = "http://127.0.0.1:8080")]
    pub url: String,
    /// Private worker identity and execution journal (must not be the server data directory).
    #[arg(long)]
    pub data_dir: Option<PathBuf>,
    /// Codex executable; useful when PATH contains a shell wrapper.
    #[arg(long, default_value = "codex")]
    pub codex_bin: PathBuf,
    #[arg(long)]
    pub model: Option<String>,
    #[arg(long, default_value="workspace-write", value_parser=["read-only", "workspace-write"])]
    pub sandbox: String,
    /// Exit after one task has been acknowledged by the server.
    #[arg(long)]
    pub once: bool,
}
#[derive(Serialize, Deserialize)]
struct Journal {
    secret: String,
    binding: String,
    task: Option<Job>,
    outcome: Option<Outcome>,
}
struct JournalFile {
    db: redb::Database,
}
impl JournalFile {
    const TABLE: redb::TableDefinition<'static, &'static str, &'static [u8]> =
        redb::TableDefinition::new("worker_journal");
    fn open(dir: &Path, binding: &str) -> Result<(Self, Journal)> {
        std::fs::create_dir_all(dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
        let file = Self {
            db: redb::Database::create(dir.join("worker.redb"))
                .context("worker identity already in use, or journal unavailable")?,
        };
        let tx = file.db.begin_write()?;
        let journal = {
            let table = tx.open_table(Self::TABLE)?;
            match table.get("state")? {
                Some(value) => serde_json::from_slice(value.value())?,
                None => Journal {
                    secret: Uuid::new_v4().to_string(),
                    binding: binding.into(),
                    task: None,
                    outcome: None,
                },
            }
        };
        tx.commit()?;
        ensure!(
            journal.binding == binding,
            "worker data directory belongs to another server, room or repository"
        );
        file.save(&journal)?;
        Ok((file, journal))
    }
    fn save(&self, journal: &Journal) -> Result<()> {
        let tx = self.db.begin_write()?;
        {
            let mut table = tx.open_table(Self::TABLE)?;
            let bytes = serde_json::to_vec(journal)?;
            table.insert("state", bytes.as_slice())?;
        }
        tx.commit()?;
        Ok(())
    }
}
struct Api {
    client: reqwest::Client,
    url: String,
    secret: String,
    runner: Uuid,
}
impl Api {
    async fn call(&self, action: Action) -> Result<Value> {
        let response = self
            .client
            .post(&self.url)
            .json(&Request {
                secret: self.secret.clone(),
                runner: self.runner,
                action,
            })
            .send()
            .await?;
        let status = response.status();
        let body = response.text().await?;
        ensure!(status.is_success(), "worker server {status}: {body}");
        Ok(serde_json::from_str(&body)?)
    }
}
fn failure(message: impl Into<String>) -> Outcome {
    let mut output = message.into();
    if output.len() > 256 * 1024 {
        let mut end = 256 * 1024;
        while !output.is_char_boundary(end) {
            end -= 1;
        }
        output.truncate(end);
    }
    Outcome {
        success: false,
        output,
    }
}

pub async fn run(mut options: Options) -> Result<()> {
    options.cwd = options
        .cwd
        .canonicalize()
        .context("worker cwd does not exist")?;
    ensure!(options.cwd.is_dir(), "worker cwd must be a directory");
    let base = options.url.trim_end_matches('/').trim_end_matches("/mcp");
    let mut url = reqwest::Url::parse(base)?;
    ensure!(
        matches!(url.scheme(), "http" | "https"),
        "worker URL must be HTTP(S)"
    );
    url.set_path("/worker");
    url.set_query(None);
    url.set_fragment(None);
    let binding = format!(
        "{}\n{}\n{}\n{}",
        url,
        options.room,
        options.agent,
        options.cwd.display()
    );
    let data_dir = options.data_dir.clone().unwrap_or_else(|| {
        crate::default_data_dir()
            .with_file_name("buddies-workers")
            .join(worker_id(&binding).replace(':', "-"))
    });
    let (file, mut journal) = JournalFile::open(&data_dir, &binding)?;
    if journal.task.is_some() && journal.outcome.is_none() {
        journal.outcome = Some(failure(
            "Worker restarted during execution; task was not repeated because it may have produced side effects.",
        ));
        file.save(&journal)?;
    }
    let api = Api {
        client: reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()?,
        url: url.into(),
        secret: journal.secret.clone(),
        runner: Uuid::new_v4(),
    };
    let stop = tokio_util::sync::CancellationToken::new();
    let token = stop.clone();
    let signal = tokio::spawn(async move {
        crate::shutdown_signal().await;
        token.cancel();
    });
    eprintln!(
        "buddies worker {} room={} cwd={} journal={}",
        worker_id(&journal.secret),
        options.room,
        options.cwd.display(),
        data_dir.display()
    );
    let result = work_loop(&options, &api, &file, &mut journal, &stop).await;
    let _ = api.call(Action::Unregister).await;
    signal.abort();
    result
}

async fn work_loop(
    options: &Options,
    api: &Api,
    file: &JournalFile,
    journal: &mut Journal,
    stop: &tokio_util::sync::CancellationToken,
) -> Result<()> {
    let mut register_at = tokio::time::Instant::now();
    loop {
        if stop.is_cancelled() {
            break;
        }
        if tokio::time::Instant::now() >= register_at {
            let register = api.call(Action::Register {
                room: options.room.clone(),
                agent: options.agent.clone(),
            });
            let registered =
                tokio::select! { _ = stop.cancelled() => break, result = register => result };
            if let Err(e) = registered {
                eprintln!("worker reconnecting: {e}");
                tokio::select! { _ = stop.cancelled() => break, _ = tokio::time::sleep(Duration::from_secs(2)) => {} }
                continue;
            }
            register_at = tokio::time::Instant::now() + Duration::from_secs(10);
        }
        if let Some(task) = journal.task.clone()
            && let Some(outcome) = journal.outcome.clone()
        {
            match api
                .call(Action::Complete {
                    task_id: task.id,
                    claim: task.claim.context("missing task claim")?,
                    outcome,
                })
                .await
            {
                Ok(_) => {
                    eprintln!("task {} acknowledged", task.id);
                    journal.task = None;
                    journal.outcome = None;
                    file.save(journal)?;
                    if options.once {
                        break;
                    }
                }
                Err(e) => {
                    eprintln!("result retained for retry: {e}");
                    tokio::select! { _ = stop.cancelled() => break, _ = tokio::time::sleep(Duration::from_secs(2)) => {} }
                    continue;
                }
            }
        }
        let claimed = tokio::select! { _ = stop.cancelled() => break, result = api.call(Action::Claim) => result };
        match claimed {
            Ok(value) if !value["task"].is_null() => {
                let task: Job = serde_json::from_value(value["task"].clone())?;
                journal.task = Some(task.clone());
                journal.outcome = None;
                file.save(journal)?;
                eprintln!("task {} running", task.id);
                let outcome = execute(options, &task, api, stop)
                    .await
                    .unwrap_or_else(|e| failure(format!("Codex execution failed: {e:#}")));
                journal.outcome = Some(outcome);
                file.save(journal)?;
                // Send the result even on graceful shutdown, within the HTTP timeout.
                if stop.is_cancelled() {
                    if api
                        .call(Action::Complete {
                            task_id: task.id,
                            claim: task.claim.unwrap(),
                            outcome: journal.outcome.clone().unwrap(),
                        })
                        .await
                        .is_ok()
                    {
                        journal.task = None;
                        journal.outcome = None;
                        file.save(journal)?;
                    }
                    break;
                }
            }
            Ok(_) => {
                tokio::select! { _ = stop.cancelled() => break, _ = tokio::time::sleep(Duration::from_millis(500)) => {} }
            }
            Err(e) => {
                eprintln!("worker polling failed: {e}");
                tokio::select! { _ = stop.cancelled() => break, _ = tokio::time::sleep(Duration::from_secs(2)) => {} }
            }
        }
    }
    Ok(())
}

async fn execute(
    options: &Options,
    task: &Job,
    api: &Api,
    stop: &tokio_util::sync::CancellationToken,
) -> Result<Outcome> {
    let mut command = tokio::process::Command::new(&options.codex_bin);
    command
        .arg("app-server")
        .arg("--stdio")
        .arg("-c")
        .arg("mcp_servers.buddies.enabled=false")
        .current_dir(&options.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command.spawn().context("start Codex app-server")?;
    let group = ProcessGroup(child.id());
    let stdin = child.stdin.take().context("Codex stdin")?;
    let stdout = child.stdout.take().context("Codex stdout")?;
    let mut rpc = Rpc {
        stdin,
        stdout: BufReader::new(stdout),
        sequence: 0,
        pending: std::collections::VecDeque::new(),
    };
    let run = async {
        rpc.request("initialize", json!({"clientInfo":{"name":"buddies-worker","version":env!("CARGO_PKG_VERSION")},"capabilities":{}})).await?;
        rpc.send(json!({"method":"initialized","params":{}}))
            .await?;
        let thread = rpc.request("thread/start", json!({"cwd":options.cwd,"approvalPolicy":"never","sandbox":options.sandbox,"model":options.model,
            "developerInstructions":"Execute only the delegated task in the configured repository. Do not poll or delegate buddies tasks; the worker handles delivery and submission. If an action needs unavailable approval, report that limitation. Return a concise final result.",
            "ephemeral":true})).await?;
        let thread_id = thread["thread"]["id"]
            .as_str()
            .context("missing Codex thread ID")?;
        let turn = rpc
            .request(
                "turn/start",
                json!({"threadId":thread_id,"input":[{"type":"text","text":task.description}]}),
            )
            .await?;
        let turn_id = turn["turn"]["id"]
            .as_str()
            .context("missing Codex turn ID")?;
        let mut output = String::new();
        loop {
            let event = rpc.next().await?;
            if event.get("id").is_some() && event.get("method").is_some() {
                rpc.send(json!({"id":event["id"],"error":{"code":-32601,"message":"Unattended worker cannot provide interactive input or approval"}})).await?;
                continue;
            }
            if event["params"]["threadId"].as_str() != Some(thread_id) {
                continue;
            }
            if event["method"] == "item/completed"
                && event["params"]["item"]["type"] == "agentMessage"
                && let Some(text) = event["params"]["item"]["text"].as_str()
            {
                output.clear();
                output.push_str(text);
                ensure!(output.len() <= 256 * 1024, "Codex result exceeds 256 KiB");
            }
            if event["method"] == "turn/completed"
                && event["params"]["turn"]["id"].as_str() == Some(turn_id)
            {
                let turn = &event["params"]["turn"];
                if turn["status"] == "completed" {
                    return Ok(Outcome {
                        success: true,
                        output,
                    });
                }
                return Ok(failure(format!(
                    "Codex turn {}: {}",
                    turn["status"], turn["error"]
                )));
            }
        }
    };
    let heartbeat = async {
        let mut last_ok = tokio::time::Instant::now();
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            match api
                .call(Action::Heartbeat {
                    task_id: task.id,
                    claim: task.claim.context("missing claim")?,
                })
                .await
            {
                Ok(value) => {
                    if !value["task"]["outcome"].is_null() {
                        return Ok(failure("Task was cancelled or expired on the server"));
                    }
                    last_ok = tokio::time::Instant::now();
                }
                Err(e) => {
                    if last_ok.elapsed() >= Duration::from_secs(20) {
                        return Ok(failure(format!("Worker cannot renew task lease: {e}")));
                    }
                }
            }
        }
    };
    let result = tokio::select! {
        result = run => result,
        result = heartbeat => result,
        _ = stop.cancelled() => Ok(failure("Worker stopped during execution; task was not retried")),
        _ = tokio::time::sleep(Duration::from_secs(task.deadline.saturating_sub(now()))) => Ok(failure("Task deadline exceeded")),
    };
    drop(group);
    let _ = child.kill().await;
    let _ = child.wait().await;
    result
}
struct ProcessGroup(Option<u32>);
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(id) = self.0 {
            // Kill only the process group created for this execution, including tool children.
            let _ = std::process::Command::new("kill")
                .args(["-KILL", "--", &format!("-{id}")])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}
struct Rpc {
    stdin: tokio::process::ChildStdin,
    stdout: BufReader<tokio::process::ChildStdout>,
    sequence: u64,
    pending: std::collections::VecDeque<Value>,
}
impl Rpc {
    async fn send(&mut self, value: Value) -> Result<()> {
        self.stdin
            .write_all(serde_json::to_string(&value)?.as_bytes())
            .await?;
        self.stdin.write_all(b"\n").await?;
        self.stdin.flush().await?;
        Ok(())
    }
    async fn next(&mut self) -> Result<Value> {
        if let Some(event) = self.pending.pop_front() {
            return Ok(event);
        }
        self.read().await
    }
    async fn read(&mut self) -> Result<Value> {
        let mut line = Vec::new();
        loop {
            let chunk = self.stdout.fill_buf().await?;
            ensure!(!chunk.is_empty(), "Codex app-server closed stdout");
            let count = chunk
                .iter()
                .position(|b| *b == b'\n')
                .map(|i| i + 1)
                .unwrap_or(chunk.len());
            ensure!(
                line.len() + count <= 1024 * 1024,
                "Codex event exceeds 1 MiB"
            );
            let done = chunk[count - 1] == b'\n';
            line.extend_from_slice(&chunk[..count]);
            self.stdout.consume(count);
            if done {
                return Ok(serde_json::from_slice(&line)?);
            }
        }
    }
    async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        self.sequence += 1;
        let id = self.sequence;
        self.send(json!({"id":id,"method":method,"params":params}))
            .await?;
        loop {
            let message = self.read().await?;
            if message.get("method").is_none() && message["id"] == id {
                ensure!(
                    message.get("error").is_none(),
                    "Codex {method}: {}",
                    message["error"]
                );
                return Ok(message["result"].clone());
            }
            if message.get("id").is_some() && message.get("method").is_some() {
                self.send(json!({"id":message["id"],"error":{"code":-32601,"message":"Interactive requests are unavailable in unattended workers"}})).await?;
            } else if message.get("method").is_some() {
                ensure!(
                    self.pending.len() < 128,
                    "too many Codex startup notifications"
                );
                self.pending.push_back(message);
            }
        }
    }
}
