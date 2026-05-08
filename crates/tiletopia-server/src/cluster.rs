//! High-availability clustering with Raft-based leader election.

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
