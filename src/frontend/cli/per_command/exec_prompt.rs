//! `ExecPromptCommandFrontend` impl for the CLI.
//!
//! `container_frontend()` comes from the blanket `HasAgentFrontend` impl
//! on `CliFrontend` (see `per_command/chat.rs`), which is a supertrait of
//! `ExecPromptCommandFrontend`.

use crate::command::commands::exec_prompt::ExecPromptCommandFrontend;

use crate::frontend::cli::command_frontend::CliFrontend;

#[async_trait::async_trait]
impl ExecPromptCommandFrontend for CliFrontend {
    fn native_container_frontend(
        &mut self,
    ) -> Box<dyn crate::engine::agent_runtime::AgentFrontend> {
        Box::new(super::container_frontend_marker::CliNativeContainerProxy {
            writers: std::sync::Arc::clone(&self.native_output_writers),
        })
    }

    async fn finish_native_output(&mut self) -> Result<(), crate::command::error::CommandError> {
        let writers = {
            let mut writers = self.native_output_writers.lock().map_err(|_| {
                crate::command::error::CommandError::Other(
                    "native output writer state unavailable".into(),
                )
            })?;
            std::mem::take(&mut *writers)
        };
        let mut error = None;
        for writer in writers {
            let result = writer
                .await
                .map_err(|cause| format!("native output writer failed: {cause}"))
                .and_then(|result| {
                    result.map_err(|cause| format!("native output write failed: {cause}"))
                });
            if let Err(cause) = result {
                error.get_or_insert(cause);
            }
        }
        match error {
            Some(cause) => Err(crate::command::error::CommandError::Other(cause)),
            None => Ok(()),
        }
    }
}
