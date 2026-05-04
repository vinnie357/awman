//! `WorktreeLifecycleFrontend` impl for the TUI.

use std::path::Path;

use crate::command::commands::worktree_lifecycle::{
    ExistingWorktreeDecision, PostWorkflowWorktreeAction, PreWorktreeDecision,
    WorktreeLifecycleFrontend,
};
use crate::command::error::CommandError;
use crate::engine::message::UserMessageSink;
use crate::frontend::tui::command_frontend::TuiCommandFrontend;
use crate::frontend::tui::dialogs::{DialogRequest, DialogResponse};

impl WorktreeLifecycleFrontend for TuiCommandFrontend {
    fn ask_pre_worktree_uncommitted_files(
        &mut self,
        files: &[String],
    ) -> Result<PreWorktreeDecision, CommandError> {
        let body = format!(
            "Uncommitted files:\n{}\n\nCommit them first, use last commit, or abort?",
            files
                .iter()
                .take(10)
                .cloned()
                .collect::<Vec<_>>()
                .join("\n")
        );
        let response = self.ask_dialog(DialogRequest::Custom {
            title: "Uncommitted files".into(),
            body,
            keys: vec![
                ('c', "Commit first".into()),
                ('u', "Use last commit".into()),
                ('a', "Abort".into()),
            ],
        })?;
        Ok(match response {
            DialogResponse::Char('c') => {
                // Ask for commit message
                let msg_response = self.ask_dialog(DialogRequest::TextInput {
                    title: "Commit message".into(),
                    prompt: "Enter commit message:".into(),
                })?;
                match msg_response {
                    DialogResponse::Text(msg) if !msg.is_empty() => {
                        PreWorktreeDecision::Commit { message: msg }
                    }
                    _ => PreWorktreeDecision::UseLastCommit,
                }
            }
            DialogResponse::Char('u') => PreWorktreeDecision::UseLastCommit,
            _ => PreWorktreeDecision::Abort,
        })
    }

    fn ask_existing_worktree(
        &mut self,
        path: &Path,
        branch: &str,
    ) -> Result<ExistingWorktreeDecision, CommandError> {
        let response = self.ask_dialog(DialogRequest::Custom {
            title: "Existing worktree".into(),
            body: format!(
                "Worktree exists at {} (branch: {}).\n\nResume or recreate?",
                path.display(),
                branch
            ),
            keys: vec![('r', "Resume".into()), ('n', "Recreate".into())],
        })?;
        Ok(match response {
            DialogResponse::Char('n') => ExistingWorktreeDecision::Recreate,
            _ => ExistingWorktreeDecision::Resume,
        })
    }

    fn report_worktree_created(&mut self, path: &Path, branch: &str) {
        self.messages.info(format!(
            "Created worktree at {} on branch {}",
            path.display(),
            branch
        ));
    }

    fn ask_post_workflow_action(
        &mut self,
        branch: &str,
        _had_error: bool,
    ) -> Result<PostWorkflowWorktreeAction, CommandError> {
        let response = self.ask_dialog(DialogRequest::Custom {
            title: "Worktree action".into(),
            body: format!(
                "Workflow complete on branch '{branch}'.\n\nWhat would you like to do?"
            ),
            keys: vec![
                ('m', "Merge into main branch".into()),
                ('d', "Discard worktree".into()),
                ('k', "Keep worktree".into()),
            ],
        })?;
        Ok(match response {
            DialogResponse::Char('m') => PostWorkflowWorktreeAction::Merge,
            DialogResponse::Char('d') => PostWorkflowWorktreeAction::Discard,
            _ => PostWorkflowWorktreeAction::Keep,
        })
    }

    fn ask_worktree_commit_before_merge(
        &mut self,
        _branch: &str,
        files: &[String],
    ) -> Result<Option<String>, CommandError> {
        let body = format!(
            "Uncommitted changes on worktree:\n{}\n\nCommit before merge?",
            files
                .iter()
                .take(10)
                .cloned()
                .collect::<Vec<_>>()
                .join("\n")
        );
        let response = self.ask_dialog(DialogRequest::YesNo {
            title: "Commit before merge?".into(),
            body,
        })?;
        if matches!(
            response,
            DialogResponse::Yes | DialogResponse::Char('y')
        ) {
            let msg_response = self.ask_dialog(DialogRequest::TextInput {
                title: "Commit message".into(),
                prompt: "Enter commit message:".into(),
            })?;
            match msg_response {
                DialogResponse::Text(msg) if !msg.is_empty() => Ok(Some(msg)),
                _ => Ok(None),
            }
        } else {
            Ok(None)
        }
    }

    fn confirm_squash_merge(&mut self, branch: &str) -> Result<bool, CommandError> {
        let response = self.ask_dialog(DialogRequest::YesNo {
            title: "Squash merge?".into(),
            body: format!("Squash-merge branch '{branch}' into main branch?"),
        })?;
        Ok(matches!(
            response,
            DialogResponse::Yes | DialogResponse::Char('y')
        ))
    }

    fn confirm_worktree_cleanup(
        &mut self,
        branch: &str,
        path: &Path,
    ) -> Result<bool, CommandError> {
        let response = self.ask_dialog(DialogRequest::YesNo {
            title: "Clean up worktree?".into(),
            body: format!(
                "Delete worktree at {} and branch '{branch}'?",
                path.display()
            ),
        })?;
        Ok(matches!(
            response,
            DialogResponse::Yes | DialogResponse::Char('y')
        ))
    }

    fn report_merge_conflict(
        &mut self,
        branch: &str,
        worktree_path: &Path,
        _git_root: &Path,
    ) {
        self.messages.error_msg(format!(
            "Merge conflict on branch '{}'. Resolve manually in {}",
            branch,
            worktree_path.display()
        ));
    }

    fn report_worktree_discarded(&mut self, branch: &str) {
        self.messages
            .info(format!("Worktree for branch '{branch}' discarded"));
    }

    fn report_worktree_kept(&mut self, path: &Path, branch: &str) {
        self.messages.info(format!(
            "Worktree kept at {} (branch: {branch})",
            path.display()
        ));
    }
}
