use std::{
    io::IsTerminal,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use ratatui::{
    crossterm::event::{self, Event, KeyCode, KeyModifiers},
    layout::{Constraint, Layout},
    style::{Color, Style},
    widgets::{Block, List, ListItem, ListState, Paragraph, Row, Table, TableState},
};
use tokio::sync::watch;

use crate::status::Snapshot;

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
    Ok(serde_json::from_slice(&body)?)
}

// Peer names/statuses are untrusted terminal input.
fn clean(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
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
        terminal.draw(|frame| {
            let areas = Layout::vertical([Constraint::Length(4), Constraint::Min(3), Constraint::Length(5)]).split(frame.area());
            let state = match (&error, updated) {
                (Some(e), _) => format!("OFFLINE — {}", clean(e)),
                (None, Some(t)) => format!("LIVE — updated {}s ago", t.elapsed().as_secs()),
                _ => "Connecting…".into(),
            };
            let summary = snapshot.as_ref().map(|s| format!("{} rooms · {} MCP sessions · node {}", s.rooms.len(), s.clients.len(), s.node_id)).unwrap_or_default();
            frame.render_widget(Paragraph::new(format!("{state}\n{summary}"))
                .block(Block::bordered().title(" Buddies network monitor "))
                .style(Style::default().fg(if error.is_some() { Color::Yellow } else { Color::Cyan })), areas[0]);
            let columns = Layout::horizontal([Constraint::Percentage(30), Constraint::Percentage(70)]).split(areas[1]);
            let left = Layout::vertical([Constraint::Percentage(55), Constraint::Percentage(45)]).split(columns[0]);
            let room_items: Vec<ListItem> = if rooms.is_empty() {
                vec![ListItem::new("No rooms joined")]
            } else {
                rooms.iter().map(|r| ListItem::new(format!("{} ({}) [{}]", clean(&r.name), r.peers.len(), clean(&r.connection.state)))).collect()
            };
            let mut selection = ListState::default().with_selected((!rooms.is_empty()).then_some(room_index));
            frame.render_stateful_widget(List::new(room_items).block(Block::bordered().title(" Rooms "))
                .highlight_style(Style::default().bg(Color::DarkGray)).highlight_symbol("› "), left[0], &mut selection);
            let clients = snapshot.as_ref().map(|s| s.clients.iter().map(|c| format!("{} {} [{}]", clean(&c.name), clean(&c.version), clean(&c.id).chars().take(14).collect::<String>())).collect::<Vec<_>>().join("\n")).unwrap_or_default();
            frame.render_widget(Paragraph::new(if clients.is_empty() { "No active MCP sessions".into() } else { clients })
                .block(Block::bordered().title(" Local MCP sessions ")), left[1]);
            let rows = peers.iter().map(|p| Row::new(vec![clean(&p.name), format!("{} · {}", clean(&p.scope), clean(&p.agent)), p.presence.to_string(), format!("{}s", p.last_seen_secs), clean(p.status.as_deref().unwrap_or("—"))]));
            let title = if peers.is_empty() { " Room participants — none " } else { " Room participants — local / remote " };
            let table = Table::new(rows, [Constraint::Percentage(20), Constraint::Percentage(20), Constraint::Length(12), Constraint::Length(8), Constraint::Min(10)])
                .header(Row::new(["User", "Agent", "Presence", "Seen", "Last status"]).style(Style::default().fg(Color::Cyan)))
                .block(Block::bordered().title(title))
                .row_highlight_style(Style::default().bg(Color::DarkGray));
            let mut state = TableState::default().with_selected((!peers.is_empty()).then_some(peer_index));
            frame.render_stateful_widget(table, columns[1], &mut state);
            let footer = if error.is_some() { "Retrying automatically; displayed data may be stale. q: quit" }
                else { "↑/↓ or j/k: rooms · PgUp/PgDn: agents · q/Esc/Ctrl-C: quit\nP2P membership is last known state, not a live connection guarantee." };
            let diagnostic = rooms.get(room_index).map(|r| format!(
                "{} local agents · network {} · {} neighbors · {} retries · {}", r.peers.iter().filter(|p| p.scope == "local").count(), clean(&r.connection.state),
                r.connection.neighbors, r.connection.reconnect_attempts,
                clean(r.connection.last_error.as_deref().unwrap_or("no transport errors"))
            )).unwrap_or_default();
            frame.render_widget(Paragraph::new(format!("{footer}\n{diagnostic}")).block(Block::bordered()), areas[2]);
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
                KeyCode::Down | KeyCode::Char('j') => {
                    selected_room = rooms
                        .get((room_index + 1).min(rooms.len().saturating_sub(1)))
                        .map(|r| r.name.clone());
                    peer_index = 0;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    selected_room = rooms
                        .get(room_index.saturating_sub(1))
                        .map(|r| r.name.clone());
                    peer_index = 0;
                }
                KeyCode::PageDown => peer_index = peer_index.saturating_add(1),
                KeyCode::PageUp => peer_index = peer_index.saturating_sub(1),
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
    }
}
