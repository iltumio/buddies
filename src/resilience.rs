//! Timing policy shared by presence and reconnect supervision.
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy)]
pub struct PresenceConfig {
    pub heartbeat: Duration,
    pub suspect_after: Duration,
    pub offline_after: Duration,
}

impl Default for PresenceConfig {
    fn default() -> Self {
        Self {
            heartbeat: Duration::from_secs(10),
            suspect_after: Duration::from_secs(30),
            offline_after: Duration::from_secs(90),
        }
    }
}

impl PresenceConfig {
    pub fn from_env() -> Result<Self> {
        let config = Self {
            heartbeat: env_duration("BUDDIES_HEARTBEAT_SECS", 10)?,
            suspect_after: env_duration("BUDDIES_SUSPECT_SECS", 30)?,
            offline_after: env_duration("BUDDIES_OFFLINE_SECS", 90)?,
        };
        config.validate()?;
        Ok(config)
    }

    pub fn validate(self) -> Result<()> {
        ensure!(
            !self.heartbeat.is_zero()
                && self.heartbeat < self.suspect_after
                && self.suspect_after < self.offline_after,
            "presence timing must satisfy 0 < heartbeat < suspect < offline"
        );
        Ok(())
    }

    pub fn state(self, elapsed: Duration) -> Presence {
        if elapsed >= self.offline_after {
            Presence::Offline
        } else if elapsed >= self.suspect_after {
            Presence::Unreachable
        } else {
            Presence::Online
        }
    }
}

pub fn env_duration(name: &str, default: u64) -> Result<Duration> {
    let seconds = match std::env::var(name) {
        Ok(value) => value
            .parse::<u64>()
            .with_context(|| format!("invalid {name}"))?,
        Err(std::env::VarError::NotPresent) => default,
        Err(error) => return Err(error).with_context(|| format!("invalid {name}")),
    };
    ensure!(
        (1..=86400).contains(&seconds),
        "{name} must be between 1 and 86400 seconds"
    );
    Ok(Duration::from_secs(seconds))
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Presence {
    Online,
    Unreachable,
    Offline,
    #[default]
    Unknown,
}

impl std::fmt::Display for Presence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Online => "online",
            Self::Unreachable => "unreachable",
            Self::Offline => "offline",
            Self::Unknown => "unknown",
        })
    }
}

#[derive(Default)]
pub struct Backoff(u32);
impl Backoff {
    pub fn reset(&mut self) {
        self.0 = 0;
    }
    pub fn next(&mut self) -> Duration {
        let base = (1u64 << self.0.min(5)).min(30);
        self.0 = self.0.saturating_add(1);
        Duration::from_millis(base * 1000 + u64::from(rand::random::<u16>()) % 501)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn presence_uses_exact_local_deadlines() {
        let config = PresenceConfig::default();
        assert_eq!(config.state(Duration::from_secs(29)), Presence::Online);
        assert_eq!(config.state(Duration::from_secs(30)), Presence::Unreachable);
        assert_eq!(config.state(Duration::from_secs(90)), Presence::Offline);
        assert!(
            PresenceConfig {
                heartbeat: config.offline_after,
                ..config
            }
            .validate()
            .is_err()
        );
    }
    #[test]
    fn retry_delay_is_bounded_and_resets() {
        let mut backoff = Backoff::default();
        for base in [1, 2, 4, 8, 16, 30, 30, 30] {
            let delay = backoff.next();
            assert!(delay >= Duration::from_secs(base));
            assert!(delay <= Duration::from_millis(base * 1000 + 500));
        }
        backoff.reset();
        assert!(backoff.next() < Duration::from_secs(2));
    }
}
