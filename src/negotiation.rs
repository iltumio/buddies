//! Structured agent negotiation and conflict-avoidance agreements.
//!
//! Allows agents working on overlapping or transitively dependent files across
//! branches, worktrees, tasks, and sprints to propose operating contracts,
//! counter-propose adjustments, and reach explicit consensus.

use std::collections::BTreeMap;

use anyhow::{Result, ensure};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

pub const NEGOTIATION_RETENTION_SECS: u64 = 7 * 86_400;
const MAX_PROPOSALS: usize = 512;
const MAX_FIELD_LEN: usize = 1024;
const MAX_SUMMARY_LEN: usize = 16 * 1024;
const MAX_FILES_PER_PROPOSAL: usize = 512;
const MAX_RESPONSES: usize = 64;
const MAX_PROPOSAL_BYTES: usize = 192 * 1024;

pub fn plan_revision(plan: &str) -> String {
    data_encoding::HEXLOWER.encode(&Sha256::digest(plan.as_bytes()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum NegotiationStatus {
    #[default]
    Open,
    Agreed,
    Countered,
    Rejected,
    Resolved,
}

impl std::fmt::Display for NegotiationStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Open => write!(f, "open"),
            Self::Agreed => write!(f, "agreed"),
            Self::Countered => write!(f, "countered"),
            Self::Rejected => write!(f, "rejected"),
            Self::Resolved => write!(f, "resolved"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NegotiationDecision {
    Accept,
    Counter,
    Reject,
    Resolve,
}

impl std::str::FromStr for NegotiationDecision {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "accept" | "accepted" | "ack" | "agree" => Ok(Self::Accept),
            "counter" | "counter_proposal" | "countered" => Ok(Self::Counter),
            "reject" | "rejected" | "nack" => Ok(Self::Reject),
            "resolve" | "resolved" => Ok(Self::Resolve),
            other => anyhow::bail!(
                "invalid decision '{other}' (expected accept, counter, reject, or resolve)"
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct NegotiationResponse {
    #[schemars(with = "String")]
    pub id: Uuid,
    /// Hash of the exact plan this decision addresses.
    pub revision: String,
    pub responder_identity: Option<String>,
    pub responder: String,
    pub agent: String,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub worktree: Option<String>,
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub sprint: Option<String>,
    pub decision: NegotiationDecision,
    pub message: String,
    #[serde(default)]
    pub counter_plan: Option<String>,
    pub timestamp: u64,
}

impl NegotiationResponse {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.revision.len() == 64 && self.revision.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid plan revision"
        );
        ensure!(
            self.responder_identity
                .as_ref()
                .is_none_or(|v| v.len() <= MAX_FIELD_LEN && !v.contains('\0')),
            "invalid responder identity"
        );
        ensure!(
            self.decision != NegotiationDecision::Counter
                || self
                    .counter_plan
                    .as_ref()
                    .is_some_and(|p| !p.trim().is_empty()),
            "counter requires a nonempty plan"
        );
        ensure!(
            self.decision == NegotiationDecision::Counter || self.counter_plan.is_none(),
            "only a counter may introduce a new plan"
        );
        for s in [&self.responder, &self.agent] {
            ensure!(
                !s.trim().is_empty() && s.len() <= MAX_FIELD_LEN && !s.as_bytes().contains(&0),
                "invalid responder/agent field"
            );
        }
        for opt in [&self.branch, &self.worktree, &self.task_id, &self.sprint]
            .into_iter()
            .flatten()
        {
            ensure!(
                opt.len() <= MAX_FIELD_LEN && !opt.as_bytes().contains(&0),
                "optional metadata field exceeds length cap or contains NUL"
            );
        }
        ensure!(
            !self.message.trim().is_empty()
                && self.message.len() <= MAX_SUMMARY_LEN
                && !self.message.as_bytes().contains(&0),
            "response message must be 1..16384 bytes without NUL"
        );
        if let Some(plan) = &self.counter_plan {
            ensure!(
                plan.len() <= MAX_SUMMARY_LEN && !plan.as_bytes().contains(&0),
                "counter_plan exceeds 16384 bytes or contains NUL"
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct NegotiationProposal {
    #[schemars(with = "String")]
    pub id: Uuid,
    pub room: String,
    pub repo: String,
    pub proposer: String,
    #[serde(default)]
    pub proposer_identity: Option<String>,
    pub agent: String,
    pub branch: String,
    #[serde(default)]
    pub worktree: Option<String>,
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub sprint: Option<String>,
    pub target_files: Vec<String>,
    #[serde(default)]
    pub affected_files: Vec<String>,
    /// Strategy such as `interface_contract`, `sequential_order`, `file_ownership`, `rebase_agreement`.
    pub strategy: String,
    pub summary: String,
    pub status: NegotiationStatus,
    #[serde(default)]
    pub agreed_plan: Option<String>,
    #[serde(default)]
    pub responses: Vec<NegotiationResponse>,
    pub timestamp: u64,
}

impl NegotiationProposal {
    pub fn validate(&self) -> Result<()> {
        for s in [
            &self.room,
            &self.repo,
            &self.proposer,
            &self.agent,
            &self.branch,
            &self.strategy,
        ] {
            ensure!(
                !s.trim().is_empty() && s.len() <= MAX_FIELD_LEN && !s.as_bytes().contains(&0),
                "required negotiation field is empty, too long, or contains NUL"
            );
        }
        for opt in [&self.worktree, &self.task_id, &self.sprint]
            .into_iter()
            .flatten()
        {
            ensure!(
                opt.len() <= MAX_FIELD_LEN && !opt.as_bytes().contains(&0),
                "optional negotiation metadata exceeds cap or contains NUL"
            );
        }
        ensure!(
            !self.summary.trim().is_empty()
                && self.summary.len() <= MAX_SUMMARY_LEN
                && !self.summary.as_bytes().contains(&0),
            "negotiation summary must be 1..16384 bytes"
        );
        ensure!(
            self.target_files.len() <= MAX_FILES_PER_PROPOSAL
                && self.affected_files.len() <= MAX_FILES_PER_PROPOSAL,
            "too many target/affected files in proposal"
        );
        for f in self.target_files.iter().chain(self.affected_files.iter()) {
            ensure!(
                !f.is_empty() && f.len() <= MAX_FIELD_LEN && !f.as_bytes().contains(&0),
                "invalid file path in negotiation proposal"
            );
        }
        ensure!(
            self.agreed_plan
                .as_ref()
                .is_none_or(|p| p.len() <= MAX_SUMMARY_LEN && !p.contains('\0')),
            "invalid agreed plan"
        );
        ensure!(
            self.proposer_identity
                .as_ref()
                .is_none_or(|p| p.len() <= MAX_FIELD_LEN && !p.contains('\0')),
            "invalid proposer identity"
        );
        ensure!(
            self.responses.len() <= MAX_RESPONSES,
            "response history is full; create a new proposal"
        );
        ensure!(
            postcard::to_allocvec(self)?.len() <= MAX_PROPOSAL_BYTES,
            "proposal exceeds total wire budget"
        );
        for r in &self.responses {
            r.validate()?;
        }
        Ok(())
    }

    pub fn current_revision(&self) -> String {
        plan_revision(self.current_plan())
    }

    fn current_plan(&self) -> &str {
        self.responses
            .iter()
            .filter(|r| r.decision == NegotiationDecision::Counter)
            .max_by_key(|r| (r.timestamp, r.id))
            .and_then(|r| r.counter_plan.as_deref())
            .unwrap_or(&self.summary)
    }

    /// Derive state from the event set, never from arrival order. Accepts are tied
    /// to a plan hash, so a concurrent counter cannot silently inherit consent.
    fn reconcile(&mut self) {
        self.responses.sort_by_key(|r| (r.timestamp, r.id));
        let plan = self.current_plan().to_owned();
        let revision = plan_revision(&plan);
        let counter = self
            .responses
            .iter()
            .rev()
            .find(|r| r.decision == NegotiationDecision::Counter);
        let plan_author = counter
            .map(|r| r.responder.as_str())
            .unwrap_or(&self.proposer);
        let plan_author_identity = match counter {
            Some(response) => response.responder_identity.as_ref(),
            None => self.proposer_identity.as_ref(),
        };
        self.status = if counter.is_some() {
            NegotiationStatus::Countered
        } else {
            NegotiationStatus::Open
        };
        self.agreed_plan = None;
        for response in &self.responses {
            if response.revision != revision {
                continue;
            }
            match response.decision {
                NegotiationDecision::Accept
                    if response.responder != plan_author
                        && !(plan_author_identity.is_some()
                            && response.responder_identity.as_ref() == plan_author_identity) =>
                {
                    self.status = NegotiationStatus::Agreed;
                    self.agreed_plan = Some(plan.clone());
                }
                NegotiationDecision::Reject => {
                    self.status = NegotiationStatus::Rejected;
                    self.agreed_plan = None;
                }
                NegotiationDecision::Resolve
                    if response.responder == self.proposer
                        && response.responder_identity == self.proposer_identity =>
                {
                    self.status = NegotiationStatus::Resolved;
                    self.agreed_plan = Some(plan.clone());
                }
                _ => {}
            }
        }
    }

    pub fn touches_or_affects(&self, path: &str) -> bool {
        self.target_files.iter().any(|p| p == path) || self.affected_files.iter().any(|p| p == path)
    }
}

#[derive(Default, Serialize, Deserialize)]
pub struct NegotiationStore {
    proposals: BTreeMap<Uuid, NegotiationProposal>,
}

impl NegotiationStore {
    pub fn upsert_proposal(
        &mut self,
        mut proposal: NegotiationProposal,
        now: u64,
    ) -> Result<NegotiationProposal> {
        proposal.validate()?;
        ensure!(
            proposal.status == NegotiationStatus::Open
                && proposal.responses.is_empty()
                && proposal.agreed_plan.is_none(),
            "a proposal must be an initial open event"
        );
        self.prune(now);
        if let Some(existing) = self.proposals.get(&proposal.id) {
            let mut initial = existing.clone();
            initial.responses.clear();
            initial.status = NegotiationStatus::Open;
            initial.agreed_plan = None;
            ensure!(
                initial == proposal,
                "proposal ID is already owned by a different immutable proposal"
            );
            return Ok(existing.clone());
        }
        ensure!(
            self.proposals.len() < MAX_PROPOSALS,
            "negotiation store is full"
        );
        proposal.reconcile();
        self.proposals.insert(proposal.id, proposal.clone());
        Ok(proposal)
    }

    pub fn record_response(
        &mut self,
        room: &str,
        proposal_id: Uuid,
        response: NegotiationResponse,
        now: u64,
    ) -> Result<NegotiationProposal> {
        response.validate()?;
        self.prune(now);
        let proposal = self.proposals.get_mut(&proposal_id).ok_or_else(|| {
            anyhow::anyhow!("unknown or expired negotiation proposal {proposal_id}")
        })?;
        ensure!(
            proposal.room == room,
            "negotiation proposal belongs to a different room"
        );

        if let Some(existing) = proposal.responses.iter().find(|r| r.id == response.id) {
            ensure!(existing == &response, "response ID collision");
            return Ok(proposal.clone());
        }
        ensure!(
            response.timestamp >= proposal.timestamp,
            "response predates proposal"
        );
        if response.responder == proposal.proposer {
            ensure!(
                response.responder_identity == proposal.proposer_identity,
                "proposer identity mismatch"
            );
        }
        ensure!(
            response.decision != NegotiationDecision::Resolve
                || response.responder == proposal.proposer,
            "only the proposer can resolve a negotiation"
        );
        let mut updated = proposal.clone();
        updated.responses.push(response);
        updated.reconcile();
        updated.validate()?;
        *proposal = updated.clone();
        Ok(updated)
    }

    pub(crate) fn entries(&self) -> impl Iterator<Item = &NegotiationProposal> {
        self.proposals.values()
    }
    pub(crate) fn from_entries(entries: Vec<NegotiationProposal>) -> Self {
        Self {
            proposals: entries.into_iter().map(|p| (p.id, p)).collect(),
        }
    }
    pub fn get(&self, id: Uuid) -> Option<&NegotiationProposal> {
        self.proposals.get(&id)
    }

    pub fn list(
        &self,
        room: Option<&str>,
        repo: Option<&str>,
        sprint: Option<&str>,
        path: Option<&str>,
        now: u64,
    ) -> Vec<NegotiationProposal> {
        let mut out: Vec<NegotiationProposal> = self
            .proposals
            .values()
            .filter(|p| now.saturating_sub(p.timestamp) < NEGOTIATION_RETENTION_SECS)
            .filter(|p| room.is_none_or(|r| p.room == r))
            .filter(|p| repo.is_none_or(|r| p.repo == r))
            .filter(|p| sprint.is_none_or(|s| p.sprint.as_deref() == Some(s)))
            .filter(|p| path.is_none_or(|f| p.touches_or_affects(f)))
            .cloned()
            .collect();
        out.sort_by_key(|p| std::cmp::Reverse(p.timestamp));
        out
    }

    fn prune(&mut self, now: u64) {
        self.proposals
            .retain(|_, p| now.saturating_sub(p.timestamp) < NEGOTIATION_RETENTION_SECS);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn negotiation_lifecycle_transitions_from_open_to_countered_and_agreed() {
        let mut store = NegotiationStore::default();
        let id = Uuid::new_v4();
        let proposal = NegotiationProposal {
            id,
            room: "dev".into(),
            repo: "buddies".into(),
            proposer: "alice".into(),
            proposer_identity: None,
            agent: "claude".into(),
            branch: "feat/auth".into(),
            worktree: Some("/tmp/wt-alice".into()),
            task_id: Some("TASK-10".into()),
            sprint: Some("Sprint-2026-W44".into()),
            target_files: vec!["src/protocol.rs".into()],
            affected_files: vec!["src/room.rs".into(), "src/server.rs".into()],
            strategy: "interface_contract".into(),
            summary: "Add sprint field to FileActivityEntry".into(),
            status: NegotiationStatus::Open,
            agreed_plan: None,
            responses: vec![],
            timestamp: 100,
        };

        let created = store.upsert_proposal(proposal, 100).unwrap();
        assert_eq!(created.status, NegotiationStatus::Open);

        // Bob counters with a suggestion
        let countered = store
            .record_response(
                "dev",
                id,
                NegotiationResponse {
                    id: Uuid::new_v4(),
                    revision: created.current_revision(),
                    responder_identity: None,
                    responder: "bob".into(),
                    agent: "codex".into(),
                    branch: Some("feat/server".into()),
                    worktree: Some("/tmp/wt-bob".into()),
                    task_id: Some("TASK-11".into()),
                    sprint: Some("Sprint-2026-W44".into()),
                    decision: NegotiationDecision::Counter,
                    message: "Use #[serde(default)] on sprint so existing callers stay compatible"
                        .into(),
                    counter_plan: Some(
                        "Add #[serde(default)] pub sprint: Option<String> to FileActivityEntry"
                            .into(),
                    ),
                    timestamp: 105,
                },
                105,
            )
            .unwrap();
        assert_eq!(countered.status, NegotiationStatus::Countered);

        // Alice accepts the counter-plan
        let agreed = store
            .record_response(
                "dev",
                id,
                NegotiationResponse {
                    id: Uuid::new_v4(),
                    revision: countered.current_revision(),
                    responder_identity: None,
                    responder: "alice".into(),
                    agent: "claude".into(),
                    branch: Some("feat/auth".into()),
                    worktree: Some("/tmp/wt-alice".into()),
                    task_id: Some("TASK-10".into()),
                    sprint: Some("Sprint-2026-W44".into()),
                    decision: NegotiationDecision::Accept,
                    message: "Agreed, adding #[serde(default)]".into(),
                    counter_plan: None,
                    timestamp: 110,
                },
                110,
            )
            .unwrap();
        assert_eq!(agreed.status, NegotiationStatus::Agreed);
        assert_eq!(
            agreed.agreed_plan.as_deref(),
            Some("Add #[serde(default)] pub sprint: Option<String> to FileActivityEntry")
        );

        // Query by affected file
        let by_affected = store.list(
            Some("dev"),
            Some("buddies"),
            Some("Sprint-2026-W44"),
            Some("src/server.rs"),
            110,
        );
        assert_eq!(by_affected.len(), 1);
        assert_eq!(by_affected[0].id, id);
    }

    pub(crate) fn fixture_proposal(now: u64) -> NegotiationProposal {
        NegotiationProposal {
            id: Uuid::new_v4(),
            room: "dev".into(),
            repo: "repo".into(),
            proposer: "alice".into(),
            proposer_identity: None,
            agent: "codex".into(),
            branch: "main".into(),
            worktree: None,
            task_id: None,
            sprint: None,
            target_files: vec!["a.ts".into()],
            affected_files: vec![],
            strategy: "contract".into(),
            summary: "initial".into(),
            status: NegotiationStatus::Open,
            agreed_plan: None,
            responses: vec![],
            timestamp: now,
        }
    }
    fn response(
        proposal: &NegotiationProposal,
        who: &str,
        decision: NegotiationDecision,
        plan: Option<&str>,
    ) -> NegotiationResponse {
        NegotiationResponse {
            id: Uuid::new_v4(),
            revision: proposal.current_revision(),
            responder_identity: None,
            responder: who.into(),
            agent: "codex".into(),
            branch: None,
            worktree: None,
            task_id: None,
            sprint: None,
            decision,
            message: "response".into(),
            counter_plan: plan.map(str::to_owned),
            timestamp: proposal.timestamp + 1,
        }
    }
    #[test]
    fn concurrent_responses_converge_and_accepts_bind_to_exact_plan() {
        let proposal = fixture_proposal(100);
        let id = proposal.id;
        let accept = response(&proposal, "bob", NegotiationDecision::Accept, None);
        let counter = response(
            &proposal,
            "carol",
            NegotiationDecision::Counter,
            Some("alternative"),
        );
        let mut a = NegotiationStore::default();
        let mut b = NegotiationStore::default();
        for store in [&mut a, &mut b] {
            store.upsert_proposal(proposal.clone(), 100).unwrap();
        }
        for event in [accept.clone(), counter.clone()] {
            a.record_response("dev", id, event, 101).unwrap();
        }
        for event in [counter.clone(), accept] {
            b.record_response("dev", id, event, 101).unwrap();
        }
        assert_eq!(a.get(id), b.get(id));
        assert_eq!(a.get(id).unwrap().status, NegotiationStatus::Countered);
        let updated = a.get(id).unwrap().clone();
        let accepted = response(&updated, "alice", NegotiationDecision::Accept, None);
        let agreed = a.record_response("dev", id, accepted.clone(), 102).unwrap();
        assert_eq!(agreed.status, NegotiationStatus::Agreed);
        assert_eq!(agreed.agreed_plan.as_deref(), Some("alternative"));
        assert_eq!(a.record_response("dev", id, accepted, 102).unwrap(), agreed);
        assert_eq!(a.upsert_proposal(proposal.clone(), 102).unwrap(), agreed);
        let mut forged = proposal.clone();
        forged.proposer = "mallory".into();
        assert!(a.upsert_proposal(forged, 102).is_err());
        let mut forged = proposal;
        forged.room = "other".into();
        assert!(a.upsert_proposal(forged, 102).is_err());
    }
    #[test]
    fn proposal_caps_cover_total_size_plan_and_history() {
        let mut p = fixture_proposal(100);
        p.agreed_plan = Some("x".repeat(300_000));
        assert!(p.validate().is_err());
        p.agreed_plan = None;
        p.target_files = vec!["x".repeat(1024); 512];
        p.affected_files = p.target_files.clone();
        assert!(p.validate().is_err());
        let mut p = fixture_proposal(100);
        let r = response(&p, "bob", NegotiationDecision::Accept, None);
        p.responses = vec![r; 65];
        assert!(p.validate().is_err());
        let mut p = fixture_proposal(100);
        p.proposer_identity = Some("ssh:owner".into());
        let mut store = NegotiationStore::default();
        store.upsert_proposal(p.clone(), 100).unwrap();
        let forged = response(&p, "alice", NegotiationDecision::Resolve, None);
        assert!(store.record_response("dev", p.id, forged, 101).is_err());
    }
}
