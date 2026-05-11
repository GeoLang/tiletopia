//! High-availability clustering with Raft-based leader election.
//!
//! When the `raft` feature is enabled, uses the openraft crate for production-grade
//! Raft consensus. Otherwise, uses a built-in single-process implementation for
//! development and testing.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

/// Node state in the Raft cluster.
#[derive(Debug, Clone, PartialEq)]
pub enum NodeState {
    Follower,
    Candidate,
    Leader,
}

/// A cluster node.
#[derive(Debug, Clone)]
pub struct ClusterNode {
    pub id: String,
    pub address: String,
    pub state: NodeState,
    pub term: u64,
    pub voted_for: Option<String>,
    pub last_heartbeat: Option<Instant>,
    pub log_index: u64,
}

/// Cluster configuration.
#[derive(Debug, Clone)]
pub struct ClusterConfig {
    pub node_id: String,
    pub election_timeout: Duration,
    pub heartbeat_interval: Duration,
    pub peers: Vec<String>,
}

impl Default for ClusterConfig {
    fn default() -> Self {
        Self {
            node_id: "node-1".into(),
            election_timeout: Duration::from_millis(300),
            heartbeat_interval: Duration::from_millis(100),
            peers: Vec::new(),
        }
    }
}

/// Raft log entry.
#[derive(Debug, Clone)]
pub struct LogEntry {
    pub term: u64,
    pub index: u64,
    pub command: ClusterCommand,
}

/// Commands replicated across the cluster.
#[derive(Debug, Clone)]
pub enum ClusterCommand {
    SetValue { key: String, value: Vec<u8> },
    DeleteValue { key: String },
    AddNode { node_id: String, address: String },
    RemoveNode { node_id: String },
}

/// Vote request message.
#[derive(Debug, Clone)]
pub struct VoteRequest {
    pub term: u64,
    pub candidate_id: String,
    pub last_log_index: u64,
    pub last_log_term: u64,
}

/// Vote response message.
#[derive(Debug, Clone)]
pub struct VoteResponse {
    pub term: u64,
    pub vote_granted: bool,
}

/// Append entries (heartbeat) message.
#[derive(Debug, Clone)]
pub struct AppendEntries {
    pub term: u64,
    pub leader_id: String,
    pub prev_log_index: u64,
    pub prev_log_term: u64,
    pub entries: Vec<LogEntry>,
    pub leader_commit: u64,
}

/// The Raft state machine.
pub struct RaftNode {
    pub config: ClusterConfig,
    pub state: NodeState,
    pub current_term: u64,
    pub voted_for: Option<String>,
    pub log: Vec<LogEntry>,
    pub commit_index: u64,
    pub last_applied: u64,
    pub votes_received: usize,
    pub leader_id: Option<String>,
    state_machine: Arc<RwLock<HashMap<String, Vec<u8>>>>,
}

impl RaftNode {
    pub fn new(config: ClusterConfig) -> Self {
        Self {
            config,
            state: NodeState::Follower,
            current_term: 0,
            voted_for: None,
            log: Vec::new(),
            commit_index: 0,
            last_applied: 0,
            votes_received: 0,
            leader_id: None,
            state_machine: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Start an election (transition to Candidate).
    pub fn start_election(&mut self) -> VoteRequest {
        self.state = NodeState::Candidate;
        self.current_term += 1;
        self.voted_for = Some(self.config.node_id.clone());
        self.votes_received = 1; // Vote for self

        VoteRequest {
            term: self.current_term,
            candidate_id: self.config.node_id.clone(),
            last_log_index: self.log.len() as u64,
            last_log_term: self.log.last().map(|e| e.term).unwrap_or(0),
        }
    }

    /// Handle a vote request from another candidate.
    pub fn handle_vote_request(&mut self, req: &VoteRequest) -> VoteResponse {
        if req.term < self.current_term {
            return VoteResponse {
                term: self.current_term,
                vote_granted: false,
            };
        }

        if req.term > self.current_term {
            self.current_term = req.term;
            self.state = NodeState::Follower;
            self.voted_for = None;
        }

        let can_vote =
            self.voted_for.is_none() || self.voted_for.as_ref() == Some(&req.candidate_id);

        let log_ok = req.last_log_term >= self.log.last().map(|e| e.term).unwrap_or(0)
            && req.last_log_index >= self.log.len() as u64;

        let grant = can_vote && log_ok;
        if grant {
            self.voted_for = Some(req.candidate_id.clone());
        }

        VoteResponse {
            term: self.current_term,
            vote_granted: grant,
        }
    }

    /// Handle a vote response.
    pub fn handle_vote_response(&mut self, resp: &VoteResponse) {
        if resp.term > self.current_term {
            self.current_term = resp.term;
            self.state = NodeState::Follower;
            return;
        }

        if self.state == NodeState::Candidate && resp.vote_granted {
            self.votes_received += 1;
            #[allow(clippy::manual_div_ceil)]
            let majority = (self.config.peers.len() + 1) / 2 + 1;
            if self.votes_received >= majority {
                self.state = NodeState::Leader;
                self.leader_id = Some(self.config.node_id.clone());
            }
        }
    }

    /// Handle append entries from leader.
    pub fn handle_append_entries(&mut self, msg: &AppendEntries) -> bool {
        if msg.term < self.current_term {
            return false;
        }

        self.current_term = msg.term;
        self.state = NodeState::Follower;
        self.leader_id = Some(msg.leader_id.clone());

        // Append new entries
        for entry in &msg.entries {
            if entry.index as usize > self.log.len() {
                self.log.push(entry.clone());
            }
        }

        // Update commit index
        if msg.leader_commit > self.commit_index {
            self.commit_index = msg.leader_commit.min(self.log.len() as u64);
            self.apply_committed();
        }

        true
    }

    /// Apply committed entries to state machine.
    fn apply_committed(&mut self) {
        while self.last_applied < self.commit_index {
            self.last_applied += 1;
            if let Some(entry) = self.log.get((self.last_applied - 1) as usize) {
                let mut sm = self.state_machine.write().unwrap();
                match &entry.command {
                    ClusterCommand::SetValue { key, value } => {
                        sm.insert(key.clone(), value.clone());
                    }
                    ClusterCommand::DeleteValue { key } => {
                        sm.remove(key);
                    }
                    _ => {}
                }
            }
        }
    }

    /// Get a value from the state machine.
    pub fn get_value(&self, key: &str) -> Option<Vec<u8>> {
        self.state_machine.read().unwrap().get(key).cloned()
    }

    /// Check if this node is the leader.
    pub fn is_leader(&self) -> bool {
        self.state == NodeState::Leader
    }
}

// ─── openraft integration ────────────────────────────────────────────────────

/// openraft type configuration for TileTopia's Raft cluster.
///
/// When the `raft` feature is enabled, this defines the concrete types
/// used by openraft for node IDs, log entries, and responses.
#[cfg(feature = "raft")]
pub mod raft_types {
    use serde::{Deserialize, Serialize};

    /// Node identifier in the openraft cluster.
    pub type NodeId = u64;

    /// Node address/info for network communication.
    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
    pub struct NodeInfo {
        pub addr: String,
    }

    impl std::fmt::Display for NodeInfo {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{}", self.addr)
        }
    }

    openraft::declare_raft_types!(
        /// TileTopia Raft type configuration.
        pub TypeConfig:
            D = Request,
            R = Response,
    );

    /// A client request to be replicated through Raft.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub enum Request {
        Set { key: String, value: Vec<u8> },
        Delete { key: String },
    }

    /// Response to a client request.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub enum Response {
        Ok,
        Value(Option<Vec<u8>>),
    }

    /// Type alias for the TileTopia Raft instance.
    pub type TileTopiaRaft = openraft::Raft<TypeConfig>;

    /// In-memory Raft log + state machine storage.
    pub struct MemStore {
        log: tokio::sync::RwLock<std::collections::BTreeMap<u64, openraft::Entry<TypeConfig>>>,
        state_machine: tokio::sync::RwLock<StateMachineData>,
        vote: tokio::sync::RwLock<Option<openraft::Vote<NodeId>>>,
        committed: tokio::sync::RwLock<Option<openraft::LogId<NodeId>>>,
        snapshot: tokio::sync::RwLock<Option<StoredSnapshot>>,
    }

    /// The replicated state machine data.
    #[derive(Debug, Default, Clone, Serialize, Deserialize)]
    pub struct StateMachineData {
        pub last_applied: Option<openraft::LogId<NodeId>>,
        pub data: std::collections::HashMap<String, Vec<u8>>,
    }

    /// A stored snapshot for Raft.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct StoredSnapshot {
        pub meta: openraft::SnapshotMeta<TypeConfig>,
        pub data: Vec<u8>,
    }

    impl MemStore {
        pub fn new() -> Self {
            Self {
                log: Default::default(),
                state_machine: Default::default(),
                vote: Default::default(),
                committed: Default::default(),
                snapshot: Default::default(),
            }
        }
    }

    impl Default for MemStore {
        fn default() -> Self {
            Self::new()
        }
    }

    impl openraft::storage::RaftLogReader<TypeConfig> for std::sync::Arc<MemStore> {
        async fn try_get_log_entries<RB: std::ops::RangeBounds<u64> + Clone + std::fmt::Debug>(
            &mut self,
            range: RB,
        ) -> Result<Vec<openraft::Entry<TypeConfig>>, openraft::StorageError<TypeConfig>> {
            let log = self.log.read().await;
            Ok(log.range(range).map(|(_, v)| v.clone()).collect())
        }

        async fn read_vote(
            &mut self,
        ) -> Result<Option<openraft::Vote<NodeId>>, openraft::StorageError<TypeConfig>> {
            Ok(self.vote.read().await.clone())
        }
    }

    impl openraft::storage::RaftSnapshotBuilder<TypeConfig> for std::sync::Arc<MemStore> {
        async fn build_snapshot(
            &mut self,
        ) -> Result<openraft::Snapshot<TypeConfig>, openraft::StorageError<TypeConfig>> {
            let sm = self.state_machine.read().await;
            let data = serde_json::to_vec(&*sm).map_err(|e| {
                openraft::StorageError::read(&e.to_string())
            })?;
            let last_applied = sm.last_applied;
            let snapshot_id = format!(
                "{}-{}",
                last_applied.map(|l| l.index).unwrap_or(0),
                last_applied.map(|l| l.leader_id.term).unwrap_or(0),
            );
            let meta = openraft::SnapshotMeta {
                last_log_id: last_applied,
                last_membership: openraft::StoredMembership::default(),
                snapshot_id,
            };
            let snapshot = StoredSnapshot {
                meta: meta.clone(),
                data: data.clone(),
            };
            *self.snapshot.write().await = Some(snapshot);
            Ok(openraft::Snapshot {
                meta,
                snapshot: Box::new(std::io::Cursor::new(data)),
            })
        }
    }

    impl openraft::storage::RaftLogStorage<TypeConfig> for std::sync::Arc<MemStore> {
        type LogReader = Self;

        async fn get_log_reader(&mut self) -> Self::LogReader {
            self.clone()
        }

        async fn save_vote(
            &mut self,
            vote: &openraft::Vote<NodeId>,
        ) -> Result<(), openraft::StorageError<TypeConfig>> {
            *self.vote.write().await = Some(*vote);
            Ok(())
        }

        async fn save_committed(
            &mut self,
            committed: Option<openraft::LogId<NodeId>>,
        ) -> Result<(), openraft::StorageError<TypeConfig>> {
            *self.committed.write().await = committed;
            Ok(())
        }

        async fn read_committed(
            &mut self,
        ) -> Result<Option<openraft::LogId<NodeId>>, openraft::StorageError<TypeConfig>> {
            Ok(self.committed.read().await.clone())
        }

        async fn append<I>(
            &mut self,
            entries: I,
            callback: openraft::storage::LogFlushed<TypeConfig>,
        ) -> Result<(), openraft::StorageError<TypeConfig>>
        where
            I: IntoIterator<Item = openraft::Entry<TypeConfig>>,
        {
            let mut log = self.log.write().await;
            for entry in entries {
                log.insert(entry.log_id.index, entry);
            }
            callback.log_io_completed(Ok(()));
            Ok(())
        }

        async fn truncate(
            &mut self,
            log_id: openraft::LogId<NodeId>,
        ) -> Result<(), openraft::StorageError<TypeConfig>> {
            let mut log = self.log.write().await;
            let keys: Vec<u64> = log
                .range(log_id.index..)
                .map(|(&k, _)| k)
                .collect();
            for key in keys {
                log.remove(&key);
            }
            Ok(())
        }

        async fn purge(
            &mut self,
            log_id: openraft::LogId<NodeId>,
        ) -> Result<(), openraft::StorageError<TypeConfig>> {
            let mut log = self.log.write().await;
            let keys: Vec<u64> = log
                .range(..=log_id.index)
                .map(|(&k, _)| k)
                .collect();
            for key in keys {
                log.remove(&key);
            }
            Ok(())
        }
    }

    impl openraft::storage::RaftStateMachine<TypeConfig> for std::sync::Arc<MemStore> {
        type SnapshotBuilder = Self;

        async fn applied_state(
            &mut self,
        ) -> Result<
            (
                Option<openraft::LogId<NodeId>>,
                openraft::StoredMembership<TypeConfig>,
            ),
            openraft::StorageError<TypeConfig>,
        > {
            let sm = self.state_machine.read().await;
            Ok((sm.last_applied, openraft::StoredMembership::default()))
        }

        async fn apply<I>(
            &mut self,
            entries: I,
        ) -> Result<Vec<Response>, openraft::StorageError<TypeConfig>>
        where
            I: IntoIterator<Item = openraft::Entry<TypeConfig>>,
        {
            let mut sm = self.state_machine.write().await;
            let mut responses = Vec::new();
            for entry in entries {
                sm.last_applied = Some(entry.log_id);
                match entry.payload {
                    openraft::EntryPayload::Normal(ref req) => match req {
                        Request::Set { key, value } => {
                            sm.data.insert(key.clone(), value.clone());
                            responses.push(Response::Ok);
                        }
                        Request::Delete { key } => {
                            sm.data.remove(key);
                            responses.push(Response::Ok);
                        }
                    },
                    _ => responses.push(Response::Ok),
                }
            }
            Ok(responses)
        }

        async fn get_snapshot_builder(&mut self) -> Self::SnapshotBuilder {
            self.clone()
        }

        async fn begin_receiving_snapshot(
            &mut self,
        ) -> Result<Box<openraft::SnapshotMismatch>, openraft::StorageError<TypeConfig>> {
            Ok(Box::default())
        }

        async fn install_snapshot(
            &mut self,
            meta: &openraft::SnapshotMeta<TypeConfig>,
            snapshot: Box<openraft::SnapshotMismatch>,
        ) -> Result<(), openraft::StorageError<TypeConfig>> {
            Ok(())
        }

        async fn get_current_snapshot(
            &mut self,
        ) -> Result<Option<openraft::Snapshot<TypeConfig>>, openraft::StorageError<TypeConfig>>
        {
            let snap = self.snapshot.read().await;
            Ok(snap.as_ref().map(|s| openraft::Snapshot {
                meta: s.meta.clone(),
                snapshot: Box::new(std::io::Cursor::new(s.data.clone())),
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_node_is_follower() {
        let node = RaftNode::new(ClusterConfig::default());
        assert_eq!(node.state, NodeState::Follower);
        assert_eq!(node.current_term, 0);
    }

    #[test]
    fn test_start_election() {
        let mut node = RaftNode::new(ClusterConfig {
            node_id: "node-1".into(),
            peers: vec!["node-2".into(), "node-3".into()],
            ..Default::default()
        });
        let req = node.start_election();
        assert_eq!(node.state, NodeState::Candidate);
        assert_eq!(node.current_term, 1);
        assert_eq!(req.candidate_id, "node-1");
    }

    #[test]
    fn test_vote_granted() {
        let mut node = RaftNode::new(ClusterConfig {
            node_id: "node-2".into(),
            ..Default::default()
        });
        let req = VoteRequest {
            term: 1,
            candidate_id: "node-1".into(),
            last_log_index: 0,
            last_log_term: 0,
        };
        let resp = node.handle_vote_request(&req);
        assert!(resp.vote_granted);
    }

    #[test]
    fn test_become_leader_with_majority() {
        let mut node = RaftNode::new(ClusterConfig {
            node_id: "node-1".into(),
            peers: vec!["node-2".into(), "node-3".into()],
            ..Default::default()
        });
        node.start_election();
        // Get one more vote (self + 1 = majority of 3)
        node.handle_vote_response(&VoteResponse {
            term: 1,
            vote_granted: true,
        });
        assert_eq!(node.state, NodeState::Leader);
    }

    #[test]
    fn test_append_entries_updates_state() {
        let mut node = RaftNode::new(ClusterConfig::default());
        let msg = AppendEntries {
            term: 2,
            leader_id: "leader-1".into(),
            prev_log_index: 0,
            prev_log_term: 0,
            entries: vec![LogEntry {
                term: 2,
                index: 1,
                command: ClusterCommand::SetValue {
                    key: "k1".into(),
                    value: b"v1".to_vec(),
                },
            }],
            leader_commit: 1,
        };
        let ok = node.handle_append_entries(&msg);
        assert!(ok);
        assert_eq!(node.current_term, 2);
        assert_eq!(node.get_value("k1"), Some(b"v1".to_vec()));
    }
}
