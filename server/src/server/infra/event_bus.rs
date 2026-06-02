// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

//! Broadcast event bus for real-time UI updates.
//!
//! The `EventBus` publishes domain events to all connected SSE clients via a
//! `tokio::sync::broadcast` channel. Each event is wrapped in an `Envelope`
//! carrying a monotonic sequence number for gap detection.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::server::api::repositories::RepoResponse;
use crate::server::api::roles::RoleResponse;
use crate::server::api::stores::StoreResponse;
use crate::server::api::system::HealthResponse;
use crate::server::api::users::UserResponse;
use crate::server::config::settings::Settings;
use crate::server::infra::task::TaskInfo;

// ---------------------------------------------------------------------------
// Topic
// ---------------------------------------------------------------------------

/// Topic discriminator for client-side event filtering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Topic {
    Repos,
    Stores,
    Tasks,
    Users,
    Roles,
    Settings,
    Health,
}

/// All topics that do not require admin privileges.
pub fn public_topics() -> HashSet<Topic> {
    [
        Topic::Repos,
        Topic::Stores,
        Topic::Tasks,
        Topic::Settings,
        Topic::Health,
    ]
    .into_iter()
    .collect()
}

/// All topics including admin-only ones.
pub fn all_topics() -> HashSet<Topic> {
    let mut topics = public_topics();
    topics.insert(Topic::Users);
    topics.insert(Topic::Roles);
    topics
}

/// Parse a comma-separated topic string into a set. Unknown names are ignored.
/// Admin-only topics are filtered out when `is_admin` is false.
pub fn parse_topics(raw: Option<&str>, is_admin: bool) -> HashSet<Topic> {
    match raw {
        None | Some("") => {
            if is_admin {
                all_topics()
            } else {
                public_topics()
            }
        }
        Some(s) => s
            .split(',')
            .filter_map(|t| {
                let topic = match t.trim() {
                    "repos" => Some(Topic::Repos),
                    "stores" => Some(Topic::Stores),
                    "tasks" => Some(Topic::Tasks),
                    "users" => Some(Topic::Users),
                    "roles" => Some(Topic::Roles),
                    "settings" => Some(Topic::Settings),
                    "health" => Some(Topic::Health),
                    _ => None,
                };
                // Filter admin-only topics for non-admin users.
                match topic {
                    Some(Topic::Users | Topic::Roles) if !is_admin => None,
                    other => other,
                }
            })
            .collect(),
    }
}

// ---------------------------------------------------------------------------
// ModelEvent
// ---------------------------------------------------------------------------

/// Domain events published to the broadcast bus.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ModelEvent {
    // --- Repos ---
    RepoCreated {
        repo: RepoResponse,
    },
    RepoUpdated {
        repo: RepoResponse,
    },
    RepoDeleted {
        name: String,
    },
    /// Stats update (artifact count / total bytes changed). Emitted by the
    /// periodic state scanner when root DirEntry values change in KV.
    RepoStatsUpdated {
        name: String,
        artifact_count: u64,
        total_bytes: u64,
    },

    // --- Stores ---
    StoreCreated {
        store: StoreResponse,
    },
    StoreUpdated {
        store: StoreResponse,
    },
    StoreDeleted {
        name: String,
    },
    /// Stats update (blob count / total bytes changed). Emitted by the
    /// periodic state scanner when store stats values change in KV.
    StoreStatsUpdated {
        name: String,
        blob_count: u64,
        total_bytes: u64,
    },

    // --- Tasks ---
    TaskCreated {
        task: TaskInfo,
    },
    /// Full task info update on lifecycle transitions and progress changes.
    /// Emitted by the periodic state scanner when task records change in KV.
    TaskUpdated {
        task: TaskInfo,
    },
    TaskDeleted {
        id: String,
    },

    // --- Users (admin-only) ---
    UserCreated {
        user: UserResponse,
    },
    UserUpdated {
        user: UserResponse,
    },
    UserDeleted {
        username: String,
    },

    // --- Roles (admin-only) ---
    RoleCreated {
        role: RoleResponse,
    },
    RoleUpdated {
        role: RoleResponse,
    },
    RoleDeleted {
        name: String,
    },

    // --- Settings ---
    SettingsUpdated {
        settings: Settings,
    },

    // --- System ---
    /// Tells the client to discard local state and re-fetch the snapshot.
    Resync,
}

// ---------------------------------------------------------------------------
// Envelope
// ---------------------------------------------------------------------------

/// Monotonic wrapper around every event published to the bus.
#[derive(Debug, Clone, Serialize)]
pub struct Envelope {
    pub seq: u64,
    pub timestamp: DateTime<Utc>,
    pub topic: Topic,
    pub event: ModelEvent,
}

impl Envelope {
    /// SSE event-type string used for client-side dispatch.
    pub fn event_type(&self) -> &'static str {
        match &self.event {
            ModelEvent::RepoCreated { .. } => "repo_created",
            ModelEvent::RepoUpdated { .. } => "repo_updated",
            ModelEvent::RepoDeleted { .. } => "repo_deleted",
            ModelEvent::RepoStatsUpdated { .. } => "repo_stats_updated",
            ModelEvent::StoreCreated { .. } => "store_created",
            ModelEvent::StoreUpdated { .. } => "store_updated",
            ModelEvent::StoreDeleted { .. } => "store_deleted",
            ModelEvent::StoreStatsUpdated { .. } => "store_stats_updated",
            ModelEvent::TaskCreated { .. } => "task_created",
            ModelEvent::TaskUpdated { .. } => "task_updated",
            ModelEvent::TaskDeleted { .. } => "task_deleted",
            ModelEvent::UserCreated { .. } => "user_created",
            ModelEvent::UserUpdated { .. } => "user_updated",
            ModelEvent::UserDeleted { .. } => "user_deleted",
            ModelEvent::RoleCreated { .. } => "role_created",
            ModelEvent::RoleUpdated { .. } => "role_updated",
            ModelEvent::RoleDeleted { .. } => "role_deleted",
            ModelEvent::SettingsUpdated { .. } => "settings_updated",
            ModelEvent::Resync => "resync",
        }
    }
}

// ---------------------------------------------------------------------------
// ModelSnapshot
// ---------------------------------------------------------------------------

/// Full state sent to a newly connected SSE client.
#[derive(Debug, Clone, Serialize)]
pub struct ModelSnapshot {
    pub seq: u64,
    pub repos: Vec<RepoResponse>,
    pub stores: Vec<StoreResponse>,
    pub tasks: Vec<TaskInfo>,
    /// `None` for non-admin users.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub users: Option<Vec<UserResponse>>,
    /// `None` for non-admin users.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub roles: Option<Vec<RoleResponse>>,
    pub settings: Settings,
    pub health: HealthResponse,
}

// ---------------------------------------------------------------------------
// Materialized model (in-memory, lock-free via ArcSwap)
// ---------------------------------------------------------------------------

/// Data visible to all authenticated users.
#[derive(Debug, Clone, Serialize)]
pub struct PublicModel {
    pub seq: u64,
    pub repos: Vec<RepoResponse>,
    pub stores: Vec<StoreResponse>,
    pub tasks: Vec<TaskInfo>,
    pub settings: Settings,
    pub health: HealthResponse,
}

/// Data visible only to admin users.
#[derive(Debug, Clone, Serialize)]
pub struct AdminModel {
    pub users: Vec<UserResponse>,
    pub roles: Vec<RoleResponse>,
}

/// Combined materialized model stored inside `ArcSwap`.
/// Public and admin halves are separately `Arc`-wrapped so non-admin reads
/// never clone admin data.
#[derive(Debug, Clone)]
pub struct MaterializedModel {
    pub public: Arc<PublicModel>,
    pub admin: Arc<AdminModel>,
}

impl MaterializedModel {
    /// Empty model used as a placeholder before the materializer loads from KV.
    pub fn empty() -> Self {
        Self {
            public: Arc::new(PublicModel {
                seq: 0,
                repos: Vec::new(),
                stores: Vec::new(),
                tasks: Vec::new(),
                settings: Settings::default(),
                health: HealthResponse {
                    status: "ok".to_string(),
                    version: crate::VERSION.to_string(),
                },
            }),
            admin: Arc::new(AdminModel {
                users: Vec::new(),
                roles: Vec::new(),
            }),
        }
    }

    /// Build a wire-format snapshot for an SSE client.
    pub fn to_snapshot(&self, is_admin: bool) -> ModelSnapshot {
        ModelSnapshot {
            seq: self.public.seq,
            repos: self.public.repos.clone(),
            stores: self.public.stores.clone(),
            tasks: self.public.tasks.clone(),
            users: if is_admin {
                Some(self.admin.users.clone())
            } else {
                None
            },
            roles: if is_admin {
                Some(self.admin.roles.clone())
            } else {
                None
            },
            settings: self.public.settings.clone(),
            health: self.public.health.clone(),
        }
    }
}

/// Lock-free handle to the materialized model, readable without blocking.
/// Analogous to `SettingsHandle`.
pub struct ModelHandle {
    inner: arc_swap::ArcSwap<MaterializedModel>,
}

impl ModelHandle {
    pub fn new(initial: MaterializedModel) -> Self {
        Self {
            inner: arc_swap::ArcSwap::from_pointee(initial),
        }
    }

    /// Lock-free read of the current model.
    pub fn load(&self) -> arc_swap::Guard<Arc<MaterializedModel>> {
        self.inner.load()
    }

    /// Swap in a new model version. Called only by the materializer.
    pub fn store(&self, new: Arc<MaterializedModel>) {
        self.inner.store(new);
    }
}

// ---------------------------------------------------------------------------
// EventBus
// ---------------------------------------------------------------------------

/// Broadcast event bus. Publish domain events; SSE handlers subscribe.
pub struct EventBus {
    tx: broadcast::Sender<Envelope>,
    seq: AtomicU64,
}

impl EventBus {
    /// Create a new event bus with the given channel capacity.
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self {
            tx,
            seq: AtomicU64::new(0),
        }
    }

    /// Publish an event. Returns the assigned sequence number.
    /// Silently drops the event if there are no active receivers.
    pub fn publish(&self, topic: Topic, event: ModelEvent) -> u64 {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let envelope = Envelope {
            seq,
            timestamp: Utc::now(),
            topic,
            event,
        };
        let _ = self.tx.send(envelope);
        seq
    }

    /// Get a new receiver for SSE streaming.
    pub fn subscribe(&self) -> broadcast::Receiver<Envelope> {
        self.tx.subscribe()
    }

    /// Current head sequence number.
    pub fn current_seq(&self) -> u64 {
        self.seq.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_topics_empty() {
        let topics = parse_topics(None, false);
        assert_eq!(topics, public_topics());
    }

    #[test]
    fn test_parse_topics_admin() {
        let topics = parse_topics(None, true);
        assert!(topics.contains(&Topic::Users));
        assert!(topics.contains(&Topic::Roles));
    }

    #[test]
    fn test_parse_topics_specific() {
        let topics = parse_topics(Some("repos,tasks,settings"), false);
        assert_eq!(topics.len(), 3);
        assert!(topics.contains(&Topic::Repos));
        assert!(topics.contains(&Topic::Tasks));
        assert!(topics.contains(&Topic::Settings));
    }

    #[test]
    fn test_parse_topics_filters_admin_for_non_admin() {
        let topics = parse_topics(Some("repos,users,roles"), false);
        assert_eq!(topics.len(), 1);
        assert!(topics.contains(&Topic::Repos));
        assert!(!topics.contains(&Topic::Users));
    }

    #[test]
    fn test_event_bus_publish_subscribe() {
        let bus = EventBus::new(16);
        let mut rx = bus.subscribe();

        let seq = bus.publish(
            Topic::Repos,
            ModelEvent::RepoDeleted {
                name: "test".into(),
            },
        );
        assert_eq!(seq, 0);

        let envelope = rx.try_recv().unwrap();
        assert_eq!(envelope.seq, 0);
        assert_eq!(envelope.topic, Topic::Repos);
        assert_eq!(envelope.event_type(), "repo_deleted");
    }

    #[test]
    fn test_event_bus_sequence_increments() {
        let bus = EventBus::new(16);
        bus.publish(Topic::Repos, ModelEvent::RepoDeleted { name: "a".into() });
        bus.publish(Topic::Repos, ModelEvent::RepoDeleted { name: "b".into() });
        assert_eq!(bus.current_seq(), 2);
    }
}
