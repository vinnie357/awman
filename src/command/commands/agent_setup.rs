//! `AgentSetupFrontend` — Layer 2 lifecycle decision: download / build the
//! requested agent, fall back to default, or abort.

use crate::command::error::CommandError;
use crate::data::session::AgentName;
use crate::engine::message::UserMessageSink;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentSetupDecision {
    Setup,
    FallbackToDefault,
    Abort,
}

pub trait AgentSetupFrontend: UserMessageSink + Send + Sync {
    fn ask_agent_setup(
        &mut self,
        requested: &AgentName,
        default: &AgentName,
        default_available: bool,
        image_only: bool,
    ) -> Result<AgentSetupDecision, CommandError>;

    fn record_fallback(&mut self, requested: &AgentName, fallback: &AgentName);
}
