use rmcp::schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub enum MemoryKind {
    Decision,
    Implementation,
    Context,
    Skill,
    Status,
}

impl std::fmt::Display for MemoryKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Decision => write!(f, "decision"),
            Self::Implementation => write!(f, "implementation"),
            Self::Context => write!(f, "context"),
            Self::Skill => write!(f, "skill"),
            Self::Status => write!(f, "status"),
        }
    }
}

impl std::str::FromStr for MemoryKind {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "decision" => Ok(Self::Decision),
            "implementation" => Ok(Self::Implementation),
            "context" => Ok(Self::Context),
            "skill" => Ok(Self::Skill),
            "status" => Ok(Self::Status),
            _ => Err(anyhow::anyhow!("unknown memory kind: {s}")),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub id: Uuid,
    pub author: String,
    pub timestamp: u64,
    pub room: String,
    pub kind: MemoryKind,
    pub title: String,
    pub content: String,
    pub tags: Vec<String>,
    pub references: Vec<Uuid>,
}

impl MemoryEntry {
    pub fn matches_query(&self, query: &str) -> bool {
        let q = query.to_lowercase();
        self.title.to_lowercase().contains(&q)
            || self.content.to_lowercase().contains(&q)
            || self.tags.iter().any(|t| t.to_lowercase().contains(&q))
    }

    pub fn matches_filters(&self, filters: &SearchFilters) -> bool {
        if let Some(ref room) = filters.room {
            if &self.room != room {
                return false;
            }
        }
        if let Some(ref kind) = filters.kind {
            let kind_str = kind.to_lowercase();
            let self_kind_str = self.kind.to_string();
            if self_kind_str != kind_str {
                return false;
            }
        }
        if let Some(ref tags) = filters.tags {
            let has_any = tags.iter().any(|t| self.tags.contains(t));
            if !has_any && !tags.is_empty() {
                return false;
            }
        }
        true
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct SearchFilters {
    pub room: Option<String>,
    pub kind: Option<String>,
    pub tags: Option<Vec<String>>,
}
