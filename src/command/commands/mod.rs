//! `src/command/commands/` — one struct per amux command.
//!
//! Each module contains the `*Command` struct (owning every flag value and
//! engine reference it needs), its `*CommandFrontend` trait (defining the
//! exact user-input methods that command requires), and the
//! `Command` impl whose `run_with_frontend(frontend) -> *Outcome` body holds
//! all of the command's business logic.

pub mod agent_auth;
pub mod agent_setup;
pub mod auth;
pub mod chat;
pub mod claws;
pub mod command_trait;
pub mod config;
pub mod download;
pub mod exec_prompt;
pub mod exec_workflow;
pub mod headless;
pub mod implement;
pub mod implement_prompts;
pub mod init;
pub mod mount_scope;
pub mod new;
pub mod ready;
pub mod remote;
pub(super) mod remote_client;
pub mod specs;
pub mod status;
pub mod worktree_lifecycle;

pub use command_trait::Command;
