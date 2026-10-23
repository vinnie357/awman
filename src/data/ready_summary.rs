use serde::{Deserialize, Serialize};

use crate::data::step_status::StepStatus;

/// Non-secret host credential health shown by `awman ready`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentCredentialHealth {
    pub agent: String,
    pub refreshable: bool,
    pub expires_in_secs: Option<i64>,
    pub expired: bool,
    pub read_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadySummary {
    pub runtime_name: String,
    pub dockerfile: StepStatus,
    pub base_image: StepStatus,
    pub agent_image: StepStatus,
    pub local_agent: StepStatus,
    pub audit: StepStatus,
    pub image_rebuild: StepStatus,
    pub aspec_folder: StepStatus,
    pub work_items_config: StepStatus,
    #[serde(default)]
    pub agent_credentials: Vec<AgentCredentialHealth>,
    pub non_default_agent_images: Vec<(String, StepStatus)>,
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
            aspec_folder: StepStatus::Pending,
            work_items_config: StepStatus::Pending,
            agent_credentials: Vec::new(),
            non_default_agent_images: Vec::new(),
        }
    }

    pub fn failed_steps(&self) -> Vec<(&'static str, &str)> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_steps_lists_required_failures_and_ignores_skipped_audit() {
        let mut s = ReadySummary::new("apple-containers");
        s.dockerfile = StepStatus::Done;
        s.base_image = StepStatus::Failed("exited with code 64".into());
        s.agent_image = StepStatus::Failed("base missing".into());
        s.local_agent = StepStatus::Done;
        s.audit = StepStatus::Skipped;
        s.image_rebuild = StepStatus::Skipped;
        assert_eq!(
            s.failed_steps(),
            vec![
                ("Base image", "exited with code 64"),
                ("Agent image", "base missing"),
            ]
        );
    }

    #[test]
    fn failed_steps_empty_when_only_optional_steps_skipped_or_warn() {
        let mut s = ReadySummary::new("docker");
        s.dockerfile = StepStatus::Done;
        s.base_image = StepStatus::Done;
        s.agent_image = StepStatus::Warn("stale".into());
        s.local_agent = StepStatus::Done;
        s.audit = StepStatus::Skipped;
        s.image_rebuild = StepStatus::Skipped;
        assert!(s.failed_steps().is_empty());
    }
}
