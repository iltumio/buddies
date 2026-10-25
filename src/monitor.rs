use std::{
    io::IsTerminal,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use ratatui::{
    crossterm::event::{self, Event, KeyCode, KeyModifiers},
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style},
    widgets::{Block, List, ListItem, ListState, Paragraph, Row, Table, TableState, Wrap},
};
use tokio::sync::watch;

use crate::{
    conversation::Message,
    orchestrator::{connector::WireEvent, state::Control},
    status::{Agent, Snapshot},
};

fn status_url(base: &str) -> Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(base).context("invalid server URL")?;
    anyhow::ensure!(
        matches!(url.scheme(), "http" | "https"),
        "use an HTTP or HTTPS URL"
    );
    let path = url.path().trim_end_matches('/').trim_end_matches("/mcp");
    url.set_path(&format!("{path}/status"));
    url.set_query(None);
    url.set_fragment(None);
    Ok(url)
}

async fn fetch(client: &reqwest::Client, url: &reqwest::Url) -> Result<Snapshot> {
    let mut response = client.get(url.clone()).send().await?.error_for_status()?;
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        anyhow::ensure!(
            body.len() + chunk.len() <= 8 * 1024 * 1024,
            "status response is too large"
        );
        body.extend_from_slice(&chunk);
    }
    parse_snapshot(&body)
}

fn parse_snapshot(body: &[u8]) -> Result<Snapshot> {
    let value: serde_json::Value = serde_json::from_slice(body)?;
    if let Some(rooms) = value.get("rooms").and_then(|rooms| rooms.as_array()) {
        anyhow::ensure!(
            rooms.iter().all(|room| room.get("messages").is_some()),
            "server does not expose room messages; update and restart the buddies HTTP service"
        );
    }
    Ok(serde_json::from_value(value)?)
}

// Peer names/statuses/messages are untrusted terminal input.
fn clean(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

fn clean_multiline(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c == '\n' {
                '\n'
            } else if c.is_control() {
                ' '
            } else {
                c
            }
        })
        .collect()
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn format_age(now: u64, ts: u64) -> String {
    let secs = now.saturating_sub(ts);
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Rooms,
    Participants,
    Messages,
}

impl Focus {
    fn next(self) -> Self {
        match self {
            Self::Rooms => Self::Participants,
            Self::Participants => Self::Messages,
            Self::Messages => Self::Rooms,
        }
    }

    fn prev(self) -> Self {
        match self {
            Self::Rooms => Self::Messages,
            Self::Participants => Self::Rooms,
            Self::Messages => Self::Participants,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Rooms => "Rooms",
            Self::Participants => "Participants",
            Self::Messages => "Messages",
        }
    }
}

#[derive(Debug, Clone)]
struct MessageView {
    is_heartbeat: bool,
    kind_label: String,
    summary: String,
    detail_body: String,
}

fn format_message_view(msg: &Message) -> MessageView {
    if let Ok(wire) = serde_json::from_str::<WireEvent>(&msg.content) {
        match wire {
            WireEvent::Dispatch { assignment, .. } => {
                let claims: Vec<String> = assignment
                    .resources
                    .iter()
                    .map(|c| {
                        if c.scopes.is_empty() {
                            c.resource.clone()
                        } else {
                            format!("{}[{}]", c.resource, c.scopes.join(","))
                        }
                    })
                    .collect();
                let scope_info = if claims.is_empty() {
                    assignment.paths.join(",")
                } else {
                    claims.join(" ")
                };
                let summary = format!(
                    "[#{} rev {}] {} ({})",
                    clean(&assignment.id),
                    assignment.revision,
                    clean(&assignment.description),
                    clean(&scope_info)
                );
                let detail_body = format!(
                    "Orchestrator Dispatch:\n  Assignment: {} (rev {})\n  Owner: {}\n  Scope: {}\n  Description: {}",
                    clean(&assignment.id),
                    assignment.revision,
                    clean(&assignment.owner),
                    clean(&scope_info),
                    clean_multiline(&assignment.description),
                );
                MessageView {
                    is_heartbeat: false,
                    kind_label: "dispatch".into(),
                    summary,
                    detail_body,
                }
            }
            WireEvent::SpecialistControl { control, .. } => match control {
                Control::Accept { id, revision } => MessageView {
                    is_heartbeat: false,
                    kind_label: "accept".into(),
                    summary: format!("Accepted assignment #{id} (rev {revision})"),
                    detail_body: format!(
                        "Specialist Control [accept]:\n  Assignment: #{id}\n  Revision: {revision}"
                    ),
                },
                Control::Checkpoint {
                    id,
                    revision,
                    detail,
                } => MessageView {
                    is_heartbeat: false,
                    kind_label: "checkpoint".into(),
                    summary: format!("[#{id} rev {revision}] {}", clean(&detail)),
                    detail_body: format!(
                        "Specialist Control [checkpoint]:\n  Assignment: #{id} (rev {revision})\n  Detail: {}",
                        clean_multiline(&detail)
                    ),
                },
                Control::Report {
                    id,
                    revision,
                    summary,
                    checks,
                } => MessageView {
                    is_heartbeat: false,
                    kind_label: "report".into(),
                    summary: format!("[#{id} rev {revision}] {}", clean(&summary)),
                    detail_body: format!(
                        "Specialist Control [report]:\n  Assignment: #{id} (rev {revision})\n  Checks: {}\n  Summary: {}",
                        if checks.is_empty() {
                            "none".into()
                        } else {
                            clean(&checks.join(", "))
                        },
                        clean_multiline(&summary)
                    ),
                },
                Control::Fail {
                    id,
                    revision,
                    detail,
                } => MessageView {
                    is_heartbeat: false,
                    kind_label: "fail".into(),
                    summary: format!("[#{id} rev {revision}] {}", clean(&detail)),
                    detail_body: format!(
                        "Specialist Control [fail]:\n  Assignment: #{id} (rev {revision})\n  Detail: {}",
                        clean_multiline(&detail)
                    ),
                },
                Control::Heartbeat { id, revision } => MessageView {
                    is_heartbeat: true,
                    kind_label: "heartbeat".into(),
                    summary: format!("Heartbeat for #{id} (rev {revision})"),
                    detail_body: format!("Specialist heartbeat for #{id} (rev {revision})"),
                },
                other => {
                    let raw = serde_json::to_string(&other).unwrap_or_default();
                    MessageView {
                        is_heartbeat: false,
                        kind_label: "control".into(),
                        summary: clean(&raw),
                        detail_body: clean_multiline(&raw),
                    }
                }
            },
            WireEvent::VerificationResult {
                id,
                revision,
                verified,
                detail,
                resource_heads,
                ..
            } => {
                let status = if verified { "PASSED" } else { "FAILED" };
                let heads: Vec<String> = resource_heads
                    .iter()
                    .map(|(k, v)| format!("{k}={}", v.chars().take(8).collect::<String>()))
                    .collect();
                MessageView {
                    is_heartbeat: false,
                    kind_label: if verified {
                        "verified".into()
                    } else {
                        "verify-err".into()
                    },
                    summary: format!(
                        "[#{} rev {} {}] {}",
                        clean(&id),
                        revision,
                        status,
                        clean(&detail)
                    ),
                    detail_body: format!(
                        "Verification Result ({status}):\n  Assignment: #{}\n  Revision: {revision}\n  Resource heads: {}\n  Detail: {}",
                        clean(&id),
                        if heads.is_empty() {
                            "none".into()
                        } else {
                            clean(&heads.join(", "))
                        },
                        clean_multiline(&detail)
                    ),
                }
            }
            WireEvent::PreparePublish { resource_heads } => {
                let heads: Vec<String> = resource_heads
                    .iter()
                    .map(|(k, v)| format!("{k}={}", v.chars().take(8).collect::<String>()))
                    .collect();
                MessageView {
                    is_heartbeat: false,
                    kind_label: "prep-pub".into(),
                    summary: format!("Prepare publication ({})", clean(&heads.join(", "))),
                    detail_body: format!(
                        "Two-Phase Publish [PreparePublish]:\n  Resource heads: {}",
                        if heads.is_empty() {
                            "none".into()
                        } else {
                            clean(&heads.join(", "))
                        }
                    ),
                }
            }
            WireEvent::Publish { resource_heads } => {
                let heads: Vec<String> = resource_heads
                    .iter()
                    .map(|(k, v)| format!("{k}={}", v.chars().take(8).collect::<String>()))
                    .collect();
                MessageView {
                    is_heartbeat: false,
                    kind_label: "publish".into(),
                    summary: format!("Publish verified changes ({})", clean(&heads.join(", "))),
                    detail_body: format!(
                        "Two-Phase Publish [Publish]:\n  Resource heads: {}",
                        if heads.is_empty() {
                            "none".into()
                        } else {
                            clean(&heads.join(", "))
                        }
                    ),
                }
            }
            WireEvent::Cancel { id, revision } => MessageView {
                is_heartbeat: false,
                kind_label: "cancel".into(),
                summary: format!("Cancel assignment #{} (rev {revision})", clean(&id)),
                detail_body: format!("Cancel Assignment #{}\n  Revision: {revision}", clean(&id)),
            },
            WireEvent::WorkerState {
                active_assignment,
                inflight,
                turn_completed,
                turn_success,
                turn_output,
                ..
            } => {
                let active_str = active_assignment.as_deref().unwrap_or("none");
                MessageView {
                    is_heartbeat: !turn_completed,
                    kind_label: "state".into(),
                    summary: format!(
                        "active={} inflight={inflight} completed={turn_completed}",
                        clean(active_str)
                    ),
                    detail_body: format!(
                        "Worker Runtime State:\n  Active assignment: {}\n  Inflight: {inflight}\n  Turn completed: {turn_completed} (success={turn_success})\n  Output: {}",
                        clean(active_str),
                        if turn_output.is_empty() {
                            "none".into()
                        } else {
                            clean_multiline(&turn_output)
                        }
                    ),
                }
            }
        }
    } else {
        MessageView {
            is_heartbeat: false,
            kind_label: clean(&msg.kind),
            summary: clean(&msg.content),
            detail_body: clean_multiline(&msg.content),
        }
    }
}

fn format_agent_name(peer: &Agent) -> String {
    if let Some(info) = &peer.participant {
        let role = if info.orchestrator {
            " [orchestrator]"
        } else if info.specialist {
            " [specialist]"
        } else {
            ""
        };
        format!("{}{role}", clean(&info.name))
    } else {
        clean(&peer.name)
    }
}

fn format_agent_status(peer: &Agent) -> String {
    if let Some(status) = &peer.status {
        return clean(status);
    }
    if let Some(info) = &peer.participant {
        if !info.resources.is_empty() {
            let specs: Vec<String> = info
                .resources
                .iter()
                .map(|r| {
                    if r.scopes.is_empty() {
                        r.id.clone()
                    } else {
                        format!("{}[{}]", r.id, r.scopes.join(","))
                    }
                })
                .collect();
            return format!("resources: {}", clean(&specs.join(" ")));
        }
        if info.orchestrator {
            return "room orchestrator".into();
        }
    }
    "—".into()
}

fn kind_color(kind: &str) -> Color {
    match kind {
        "question" => Color::Yellow,
        "answer" => Color::Green,
        "request" | "dispatch" => Color::Cyan,
        "report" | "verified" | "publish" => Color::LightGreen,
        "fail" | "verify-err" | "cancel" => Color::Red,
        "accept" | "checkpoint" | "prep-pub" => Color::Blue,
        _ => Color::Gray,
    }
}

fn state_color(state: &str) -> Color {
    match state {
        "handled" => Color::Green,
        "read" => Color::Cyan,
        "failed" => Color::Red,
        _ => Color::Yellow,
    }
}

struct TerminalGuard;
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        ratatui::restore();
    }
}

struct Poller(tokio::task::JoinHandle<()>);
impl Drop for Poller {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub async fn run(base: &str, once: bool) -> Result<()> {
    let url = status_url(base)?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()?;
    if once {
        let snapshot = fetch(&client, &url)
            .await
            .context("cannot read buddies status; start its HTTP service or check --url")?;
        println!("{}", serde_json::to_string_pretty(&snapshot)?);
        return Ok(());
    }
    anyhow::ensure!(
        std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
        "monitor requires a terminal; use --once for JSON output"
    );
    let (tx, mut rx) = watch::channel(None);
    let _poller = Poller(tokio::spawn(async move {
        loop {
            let result = fetch(&client, &url).await.map_err(|e| format!("{e:#}"));
            if tx.send(Some(result)).is_err() {
                break;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }));
    let mut terminal = ratatui::init();
    let _guard = TerminalGuard;
    let mut snapshot: Option<Snapshot> = None;
    let mut error = None;
    let mut updated = None;
    let mut selected_room: Option<String> = None;
    let mut peer_index = 0usize;
    let mut msg_index = 0usize;
    let mut follow_messages = true;
    let mut show_heartbeats = false;
    let mut focus = Focus::Messages;
    let shutdown = crate::shutdown_signal();
    tokio::pin!(shutdown);
    loop {
        if rx.has_changed()? {
            match rx.borrow_and_update().as_ref().unwrap() {
                Ok(value) => {
                    snapshot = Some(value.clone());
                    updated = Some(Instant::now());
                    error = None;
                }
                Err(message) => error = Some(message.clone()),
            }
        }
        let rooms = snapshot
            .as_ref()
            .map(|s| s.rooms.as_slice())
            .unwrap_or_default();
        let room_index = selected_room
            .as_ref()
            .and_then(|name| rooms.iter().position(|r| &r.name == name))
            .unwrap_or(0);
        selected_room = rooms.get(room_index).map(|r| r.name.clone());
        let peers = rooms
            .get(room_index)
            .map(|r| r.peers.as_slice())
            .unwrap_or_default();
        peer_index = peer_index.min(peers.len().saturating_sub(1));

        let raw_messages = rooms
            .get(room_index)
            .map(|r| r.messages.as_slice())
            .unwrap_or_default();
        let visible_messages: Vec<(&Message, MessageView)> = raw_messages
            .iter()
            .map(|m| (m, format_message_view(m)))
            .filter(|(_, view)| show_heartbeats || !view.is_heartbeat)
            .collect();

        if follow_messages || msg_index >= visible_messages.len() {
            msg_index = visible_messages.len().saturating_sub(1);
        }

        let now = unix_now();
        terminal.draw(|frame| {
            let areas = Layout::vertical([
                Constraint::Length(4),
                Constraint::Min(10),
                Constraint::Length(5),
            ])
            .split(frame.area());
            let state = match (&error, updated) {
                (Some(e), _) => format!("OFFLINE — {}", clean(e)),
                (None, Some(t)) => format!("LIVE — updated {}s ago", t.elapsed().as_secs()),
                _ => "Connecting…".into(),
            };
            let total_msgs: usize = rooms.iter().map(|r| r.messages.len()).sum();
            let summary = snapshot
                .as_ref()
                .map(|s| {
                    format!(
                        "{} rooms · {} MCP sessions · {} messages · focus: {} · node {}",
                        s.rooms.len(),
                        s.clients.len(),
                        total_msgs,
                        focus.label(),
                        s.node_id
                    )
                })
                .unwrap_or_default();
            frame.render_widget(
                Paragraph::new(format!("{state}\n{summary}"))
                    .block(Block::bordered().title(" Buddies network monitor "))
                    .style(Style::default().fg(if error.is_some() {
                        Color::Yellow
                    } else {
                        Color::Cyan
                    })),
                areas[0],
            );

            let columns =
                Layout::horizontal([Constraint::Percentage(28), Constraint::Percentage(72)])
                    .split(areas[1]);
            let left = Layout::vertical([Constraint::Percentage(55), Constraint::Percentage(45)])
                .split(columns[0]);
            let right = Layout::vertical([
                Constraint::Percentage(32),
                Constraint::Percentage(40),
                Constraint::Percentage(28),
            ])
            .split(columns[1]);

            let room_border = if focus == Focus::Rooms {
                Style::default().fg(Color::Cyan)
            } else {
                Style::default()
            };
            let room_items: Vec<ListItem> = if rooms.is_empty() {
                vec![ListItem::new("No rooms joined")]
            } else {
                rooms
                    .iter()
                    .map(|r| {
                        ListItem::new(format!(
                            "{} ({} peers · {} msgs) [{}]",
                            clean(&r.name),
                            r.peers.len(),
                            r.messages.len(),
                            clean(&r.connection.state)
                        ))
                    })
                    .collect()
            };
            let mut room_selection =
                ListState::default().with_selected((!rooms.is_empty()).then_some(room_index));
            frame.render_stateful_widget(
                List::new(room_items)
                    .block(
                        Block::bordered()
                            .title(" Rooms ")
                            .border_style(room_border),
                    )
                    .highlight_style(Style::default().bg(Color::DarkGray))
                    .highlight_symbol("› "),
                left[0],
                &mut room_selection,
            );

            let clients = snapshot
                .as_ref()
                .map(|s| {
                    s.clients
                        .iter()
                        .map(|c| {
                            format!(
                                "{} {} [{}]",
                                clean(&c.name),
                                clean(&c.version),
                                clean(&c.id).chars().take(14).collect::<String>()
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default();
            frame.render_widget(
                Paragraph::new(if clients.is_empty() {
                    "No active MCP sessions".into()
                } else {
                    clients
                })
                .block(Block::bordered().title(" Local MCP sessions ")),
                left[1],
            );

            // Right Top: Participants table
            let part_border = if focus == Focus::Participants {
                Style::default().fg(Color::Cyan)
            } else {
                Style::default()
            };
            let peer_rows = peers.iter().map(|p| {
                Row::new(vec![
                    format_agent_name(p),
                    format!("{} · {}", clean(&p.scope), clean(&p.agent)),
                    p.presence.to_string(),
                    format!("{}s", p.last_seen_secs),
                    format_agent_status(p),
                ])
            });
            let part_title = if peers.is_empty() {
                " Room participants — none "
            } else {
                " Room participants — local / specialist / orchestrator / remote "
            };
            let part_table = Table::new(
                peer_rows,
                [
                    Constraint::Percentage(24),
                    Constraint::Percentage(20),
                    Constraint::Length(10),
                    Constraint::Length(7),
                    Constraint::Min(12),
                ],
            )
            .header(
                Row::new(["Participant", "Agent", "Presence", "Seen", "Status / Boundaries"])
                    .style(Style::default().fg(Color::Cyan)),
            )
            .block(
                Block::bordered()
                    .title(part_title)
                    .border_style(part_border),
            )
            .row_highlight_style(Style::default().bg(Color::DarkGray));
            let mut part_state =
                TableState::default().with_selected((!peers.is_empty()).then_some(peer_index));
            frame.render_stateful_widget(part_table, right[0], &mut part_state);

            // Right Middle: Room messages table
            let msg_border = if focus == Focus::Messages {
                Style::default().fg(Color::Cyan)
            } else {
                Style::default()
            };
            let msg_rows = visible_messages.iter().map(|(m, view)| {
                Row::new(vec![
                    format_age(now, m.timestamp),
                    format!("{} → {}", clean(&m.from), clean(&m.to)),
                    view.kind_label.clone(),
                    clean(&m.state),
                    view.summary.clone(),
                ])
                .style(Style::default().fg(kind_color(&view.kind_label)))
            });
            let follow_badge = if follow_messages { "FOLLOW" } else { "MANUAL" };
            let wire_badge = if show_heartbeats {
                "ALL-EVENTS"
            } else {
                "HIDE-HEARTBEATS"
            };
            let msg_title = format!(
                " Room messages ({}/{}) — {follow_badge} · {wire_badge} ",
                visible_messages.len(),
                raw_messages.len()
            );
            let msg_table = Table::new(
                msg_rows,
                [
                    Constraint::Length(6),
                    Constraint::Percentage(24),
                    Constraint::Length(11),
                    Constraint::Length(9),
                    Constraint::Min(20),
                ],
            )
            .header(
                Row::new(["Age", "From → To", "Kind", "Receipt", "Message / Event"])
                    .style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            )
            .block(
                Block::bordered()
                    .title(msg_title)
                    .border_style(msg_border),
            )
            .row_highlight_style(
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            );
            let mut msg_state = TableState::default()
                .with_selected((!visible_messages.is_empty()).then_some(msg_index));
            frame.render_stateful_widget(msg_table, right[1], &mut msg_state);

            // Right Bottom: Selected message detail inspector
            let (inspector_title, inspector_text, inspector_color) =
                if let Some((m, view)) = visible_messages.get(msg_index) {
                    let conv_short: String = m.conversation_id.to_string().chars().take(8).collect();
                    let reply_short = m
                        .in_reply_to
                        .map(|id| id.to_string().chars().take(8).collect::<String>())
                        .unwrap_or_else(|| "none".into());
                    let header = format!(
                        "From: {} → To: {} · Kind: {} · Receipt: {} · Age: {} · Conv: {} · Reply-To: {}",
                        clean(&m.from),
                        clean(&m.to),
                        view.kind_label,
                        clean(&m.state),
                        format_age(now, m.timestamp),
                        conv_short,
                        reply_short
                    );
                    let receipt_line = m
                        .detail
                        .as_deref()
                        .map(|d| format!("\nReceipt detail: {}", clean_multiline(d)))
                        .unwrap_or_default();
                    (
                        format!(" Message detail ({}/{}) ", msg_index + 1, visible_messages.len()),
                        format!("{header}\n{}{receipt_line}", view.detail_body),
                        state_color(&m.state),
                    )
                } else {
                    (
                        " Message detail ".to_string(),
                        "No messages exchanged in this room yet.".to_string(),
                        Color::DarkGray,
                    )
                };
            frame.render_widget(
                Paragraph::new(inspector_text)
                    .block(
                        Block::bordered()
                            .title(inspector_title)
                            .border_style(Style::default().fg(inspector_color)),
                    )
                    .wrap(Wrap { trim: false }),
                right[2],
            );

            let footer = if error.is_some() {
                "Retrying automatically; displayed data may be stale. q: quit"
            } else {
                "Tab: switch panel · ↑/↓ or j/k: navigate panel · n/p or ←/→: messages · f: follow · w: heartbeats · q: quit"
            };
            let diagnostic = rooms
                .get(room_index)
                .map(|r| {
                    format!(
                        "{} local agents · {} registered participants · network {} · {} neighbors · {} retries · {}",
                        r.peers.iter().filter(|p| p.scope == "local").count(),
                        r.participants.len(),
                        clean(&r.connection.state),
                        r.connection.neighbors,
                        r.connection.reconnect_attempts,
                        clean(
                            r.connection
                                .last_error
                                .as_deref()
                                .unwrap_or("no transport errors")
                        )
                    )
                })
                .unwrap_or_default();
            frame.render_widget(
                Paragraph::new(format!("{footer}\n{diagnostic}")).block(Block::bordered()),
                areas[2],
            );
        })?;
        if event::poll(Duration::ZERO)?
            && let Event::Key(key) = event::read()?
            && key.is_press()
        {
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => break,
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    break;
                }
                KeyCode::Tab => focus = focus.next(),
                KeyCode::BackTab => focus = focus.prev(),
                KeyCode::Char('f') => {
                    follow_messages = !follow_messages;
                    if follow_messages {
                        msg_index = visible_messages.len().saturating_sub(1);
                    }
                }
                KeyCode::Char('w') => {
                    show_heartbeats = !show_heartbeats;
                }
                KeyCode::Char('n') | KeyCode::Right => {
                    follow_messages = false;
                    msg_index = (msg_index + 1).min(visible_messages.len().saturating_sub(1));
                }
                KeyCode::Char('p') | KeyCode::Left => {
                    follow_messages = false;
                    msg_index = msg_index.saturating_sub(1);
                }
                KeyCode::Home | KeyCode::Char('g') => {
                    follow_messages = false;
                    msg_index = 0;
                }
                KeyCode::End | KeyCode::Char('G') => {
                    follow_messages = true;
                    msg_index = visible_messages.len().saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => match focus {
                    Focus::Rooms => {
                        selected_room = rooms
                            .get((room_index + 1).min(rooms.len().saturating_sub(1)))
                            .map(|r| r.name.clone());
                        peer_index = 0;
                        follow_messages = true;
                    }
                    Focus::Participants => {
                        peer_index = (peer_index + 1).min(peers.len().saturating_sub(1));
                    }
                    Focus::Messages => {
                        follow_messages = false;
                        msg_index = (msg_index + 1).min(visible_messages.len().saturating_sub(1));
                    }
                },
                KeyCode::Up | KeyCode::Char('k') => match focus {
                    Focus::Rooms => {
                        selected_room = rooms
                            .get(room_index.saturating_sub(1))
                            .map(|r| r.name.clone());
                        peer_index = 0;
                        follow_messages = true;
                    }
                    Focus::Participants => {
                        peer_index = peer_index.saturating_sub(1);
                    }
                    Focus::Messages => {
                        follow_messages = false;
                        msg_index = msg_index.saturating_sub(1);
                    }
                },
                KeyCode::PageDown => {
                    peer_index = peer_index.saturating_add(1);
                }
                KeyCode::PageUp => {
                    peer_index = peer_index.saturating_sub(1);
                }
                _ => {}
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(50)) => {},
            _ = &mut shutdown => break,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn rejects_legacy_status_instead_of_showing_an_empty_history() {
        let mut snapshot = serde_json::json!({
            "node_id": "node", "signer": null, "clients": [],
            "rooms": [{"name": "room", "peers": []}]
        });
        let error = parse_snapshot(&serde_json::to_vec(&snapshot).unwrap()).unwrap_err();
        assert!(error.to_string().contains("update and restart"));
        snapshot["rooms"][0]["messages"] = serde_json::json!([]);
        assert!(parse_snapshot(&serde_json::to_vec(&snapshot).unwrap()).is_ok());
    }

    #[test]
    fn completed_turns_remain_visible_and_unicode_heads_do_not_panic() {
        let mut message = Message {
            id: Uuid::new_v4(),
            conversation_id: Uuid::new_v4(),
            in_reply_to: None,
            room: "room".into(),
            from: "worker".into(),
            to: "lead".into(),
            kind: "update".into(),
            timestamp: 100,
            state: "handled".into(),
            detail: None,
            content: serde_json::json!({
                "orchestrator_event": "worker_state", "thread_id": null,
                "active_assignment": null, "accepted_revision": null,
                "inflight": false, "uncertain": false,
                "turn_completed": true, "turn_success": true, "turn_output": "Done"
            })
            .to_string(),
        };
        let view = format_message_view(&message);
        assert_eq!(view.kind_label, "state");
        assert!(!view.is_heartbeat);
        assert!(view.detail_body.contains("Done"));
        for event in ["prepare_publish", "publish", "verification_result"] {
            message.content = serde_json::json!({
                "orchestrator_event": event,
                "resource_heads": {"fs:docs": "abcdefg🌍revision"},
                "id": "task", "revision": 1, "verified": true, "detail": "ok"
            })
            .to_string();
            assert!(
                format_message_view(&message)
                    .detail_body
                    .contains("abcdefg🌍")
            );
        }
    }

    #[test]
    fn accepts_server_and_mcp_urls() {
        for base in [
            "http://localhost:8080",
            "http://localhost:8080/",
            "http://localhost:8080/mcp/",
        ] {
            assert_eq!(
                status_url(base).unwrap().as_str(),
                "http://localhost:8080/status"
            );
        }
        assert!(status_url("file:///tmp/status").is_err());
        assert_eq!(
            status_url("https://example.com/buddies/mcp?x=1")
                .unwrap()
                .as_str(),
            "https://example.com/buddies/status"
        );
    }

    #[test]
    fn strips_terminal_control_characters() {
        assert_eq!(clean("alice\x1b[2J\nhello\r\t"), "alice [2J hello  ");
        assert_eq!(
            clean_multiline("line1\nline2\x1b[2J\r"),
            "line1\nline2 [2J "
        );
    }

    #[test]
    fn formats_plain_and_wire_messages_for_monitor() {
        let plain = Message {
            id: Uuid::new_v4(),
            conversation_id: Uuid::new_v4(),
            in_reply_to: None,
            room: "prod".into(),
            from: "frontend".into(),
            to: "backend".into(),
            kind: "question".into(),
            content: "Which endpoint returns users?".into(),
            timestamp: 100,
            state: "handled".into(),
            detail: Some("answered".into()),
        };
        let view = format_message_view(&plain);
        assert!(!view.is_heartbeat);
        assert_eq!(view.kind_label, "question");
        assert_eq!(view.summary, "Which endpoint returns users?");

        let wire_report = WireEvent::SpecialistControl {
            control: Control::Report {
                id: "be-api".into(),
                revision: 2,
                summary: "Exposed /v2/users".into(),
                checks: vec!["cargo test".into()],
            },
            worktree: None,
            base: None,
            resource_heads: Default::default(),
        };
        let wire_msg = Message {
            content: serde_json::to_string(&wire_report).unwrap(),
            kind: "update".into(),
            ..plain
        };
        let wire_view = format_message_view(&wire_msg);
        assert!(!wire_view.is_heartbeat);
        assert_eq!(wire_view.kind_label, "report");
        assert!(
            wire_view
                .summary
                .contains("[#be-api rev 2] Exposed /v2/users")
        );
        assert!(wire_view.detail_body.contains("cargo test"));
    }
}
