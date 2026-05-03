//! `ReadySummary` — final report from a `ReadyEngine` run.

use serde::{Deserialize, Serialize};

use crate::engine::step_status::StepStatus;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadySummary {
    pub runtime_name: String,
    /// Result of writing/refreshing `Dockerfile.dev` at the git root.
    pub dockerfile: StepStatus,
    pub base_image: StepStatus,
    pub agent_image: StepStatus,
    pub local_agent: StepStatus,
    pub audit: StepStatus,
    /// Result of rebuilding the base + agent images after the audit modified
    /// `Dockerfile.dev`. `Skipped` when no rebuild was needed.
    pub image_rebuild: StepStatus,
    pub legacy_migration: StepStatus,
}

impl ReadySummary {
    pub fn new(runtime_name: impl Into<String>) -> Self {
        Self {
            runtime_name: runtime_name.into(),
            dockerfile: StepStatus::Pending,
            base_image: StepStatus::Pending,
            agent_image: StepStatus::Pending,
            local_agent: StepStatus::Pending,
            audit: StepStatus::Pending,
            image_rebuild: StepStatus::Pending,
            legacy_migration: StepStatus::Pending,
        }
    }
}
