//! Queue worker — claims commands from the SQLite queue and executes them.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::RwLock;

use crate::command::dispatch::{Dispatch, Engines};
use crate::data::execution_event::EventPayload;
use crate::data::fs::api_db::SqliteSessionStore;
use crate::data::fs::api_paths::ApiPaths;
use crate::data::session::Session;
use crate::frontend::api::command_frontend::ApiDispatchFrontend;
use crate::frontend::api::event_bus::EventBus;

pub struct QueueWorker {
    worker_id: String,
    store: Arc<SqliteSessionStore>,
    engines: Engines,
    sessions: Arc<tokio::sync::Mutex<HashMap<String, Arc<RwLock<Session>>>>>,
    event_buses: Arc<tokio::sync::Mutex<HashMap<String, Arc<EventBus>>>>,
    paths: ApiPaths,
}

impl QueueWorker {
    pub fn new(
        worker_id: String,
        store: Arc<SqliteSessionStore>,
        engines: Engines,
        sessions: Arc<tokio::sync::Mutex<HashMap<String, Arc<RwLock<Session>>>>>,
        event_buses: Arc<tokio::sync::Mutex<HashMap<String, Arc<EventBus>>>>,
        paths: ApiPaths,
    ) -> Self {
        Self {
            worker_id,
            store,
            engines,
            sessions,
            event_buses,
            paths,
        }
    }

    pub async fn run(self) {
        loop {
            let claimed = self.store.claim_next_command(&self.worker_id);
            match claimed {
                Ok(Some(cmd)) => {
                    tracing::info!(
                        worker_id = %self.worker_id,
                        command_id = %cmd.id,
                        session_id = %cmd.session_id,
                        subcommand = %cmd.subcommand,
                        "Worker claimed command"
                    );
                    self.execute_command(cmd).await;
                }
                Ok(None) => {
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                }
                Err(e) => {
                    tracing::error!(
                        worker_id = %self.worker_id,
                        error = %e,
                        "Worker failed to claim command"
                    );
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
            }
        }
    }

    async fn execute_command(&self, cmd: crate::data::fs::api_db::CommandRecord) {
        let command_id = cmd.id.clone();
        let session_id = cmd.session_id.clone();
        let subcommand = cmd.subcommand.clone();
        let args: Vec<String> = serde_json::from_str(&cmd.args).unwrap_or_default();

        // Create command directory.
        let cmd_dir = self.paths.command_dir(&session_id, &command_id);
        if let Err(e) = tokio::fs::create_dir_all(&cmd_dir).await {
            tracing::error!(
                command_id = %command_id,
                error = %e,
                "Failed to create command directory"
            );
            let result_json = serde_json::to_string(&serde_json::json!({
                "error": format!("Failed to create command directory: {e}"),
            }))
            .ok();
            let _ = self.store.complete_command(
                &command_id,
                "error",
                None,
                result_json.as_deref(),
            );
            self.post_execution_check(&session_id).await;
            return;
        }

        let log_path = cmd_dir.join("output.log");

        // Write initial metadata.
        {
            let metadata = serde_json::json!({
                "command_id": command_id,
                "session_id": session_id,
                "subcommand": subcommand,
                "args": args,
                "started_at": cmd.started_at,
                "worker_id": self.worker_id,
            });
            let meta_path = self.paths.command_metadata_path(&session_id, &command_id);
            let _ = tokio::fs::write(
                &meta_path,
                serde_json::to_string_pretty(&metadata).unwrap_or_default(),
            )
            .await;
        }

        // Create EventBus for this command execution.
        let event_bus = Arc::new(EventBus::new(4096));

        // Spawn logfile writer task.
        {
            let mut log_rx = event_bus.subscribe();
            let events_log_path = self
                .paths
                .command_events_log_path(&session_id, &command_id);
            let output_log_path = log_path.clone();
            tokio::spawn(async move {
                use tokio::io::AsyncWriteExt;
                let mut events_file = match tokio::fs::File::create(&events_log_path).await {
                    Ok(f) => f,
                    Err(e) => {
                        tracing::error!(error = %e, "Failed to create events.log");
                        return;
                    }
                };
                let mut output_file = match tokio::fs::File::create(&output_log_path).await {
                    Ok(f) => f,
                    Err(e) => {
                        tracing::error!(error = %e, "Failed to create output.log");
                        return;
                    }
                };
                loop {
                    match log_rx.recv().await {
                        Ok(event) => {
                            if let Ok(json) = serde_json::to_string(&event) {
                                let _ = events_file
                                    .write_all(format!("{json}\n").as_bytes())
                                    .await;
                            }
                            if let Some(text) = event.payload.to_plain_text() {
                                let _ = output_file
                                    .write_all(format!("{text}\n").as_bytes())
                                    .await;
                            }
                            if matches!(event.payload, EventPayload::Done) {
                                let _ = events_file.flush().await;
                                let _ = output_file.flush().await;
                                break;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!(lagged = n, "Logfile writer lagged");
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            });
        }

        // Store EventBus handle for SSE subscribers.
        self.event_buses
            .lock()
            .await
            .insert(command_id.clone(), Arc::clone(&event_bus));

        // Construct ApiDispatchFrontend.
        let frontend = ApiDispatchFrontend::new(&subcommand, &args, event_bus.sender());

        // Look up the Session.
        let session = match self.sessions.lock().await.get(&session_id).cloned() {
            Some(s) => s,
            None => {
                tracing::error!(
                    command_id = %command_id,
                    session_id = %session_id,
                    "Session not found in memory"
                );
                drop(frontend);
                let result_json = serde_json::to_string(&serde_json::json!({
                    "error": "Session not found in memory",
                }))
                .ok();
                let _ = self.store.complete_command(
                    &command_id,
                    "error",
                    None,
                    result_json.as_deref(),
                );
                self.cleanup_event_bus(&command_id).await;
                self.post_execution_check(&session_id).await;
                return;
            }
        };

        // Dispatch through Layer 2.
        let path_parts: Vec<&str> = subcommand.split_whitespace().collect();
        let dispatch = Dispatch::new(frontend, session, self.engines.clone());
        let result = dispatch.run_command(&path_parts).await;

        let (status, exit_code) = match &result {
            Ok(_) => ("done", Some(0)),
            Err(_) => ("error", Some(1)),
        };

        let result_json = match &result {
            Ok(_) => serde_json::to_string(&serde_json::json!({
                "exit_code": 0,
            }))
            .ok(),
            Err(e) => serde_json::to_string(&serde_json::json!({
                "exit_code": 1,
                "error": e.to_string(),
            }))
            .ok(),
        };

        if let Err(ref e) = result {
            tracing::error!(command_id = %command_id, error = %e, "Command failed");
        }

        let _ = self.store.complete_command(
            &command_id,
            status,
            exit_code,
            result_json.as_deref(),
        );

        // Write final metadata.
        {
            let finished_at = chrono::Utc::now().to_rfc3339();
            let metadata = serde_json::json!({
                "command_id": command_id,
                "session_id": session_id,
                "subcommand": subcommand,
                "args": args,
                "started_at": cmd.started_at,
                "finished_at": finished_at,
                "exit_code": exit_code,
                "status": status,
                "worker_id": self.worker_id,
            });
            let meta_path = self.paths.command_metadata_path(&session_id, &command_id);
            let _ = tokio::fs::write(
                &meta_path,
                serde_json::to_string_pretty(&metadata).unwrap_or_default(),
            )
            .await;
        }

        self.cleanup_event_bus(&command_id).await;
        self.post_execution_check(&session_id).await;
    }

    async fn cleanup_event_bus(&self, command_id: &str) {
        let buses = Arc::clone(&self.event_buses);
        let cmd_id = command_id.to_string();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            buses.lock().await.remove(&cmd_id);
        });
    }

    async fn post_execution_check(&self, session_id: &str) {
        // Check if the session is in 'closing' state. If so, trigger final cleanup.
        let session_record = match self.store.get_session(session_id) {
            Ok(Some(r)) => r,
            _ => return,
        };

        if session_record.status != "closing" {
            return;
        }

        // Check if there's still a running command for this session.
        match self.store.running_command_for_session(session_id) {
            Ok(Some(_)) => return, // still running, don't clean up yet
            _ => {}
        }

        tracing::info!(
            worker_id = %self.worker_id,
            session_id = %session_id,
            "Session is closing and no running commands remain; running final cleanup"
        );

        // For remote sessions, delete the cloned directory.
        if session_record.session_type == "remote" {
            if let Some(ref cloned_path) = session_record.cloned_path {
                let path = std::path::PathBuf::from(cloned_path);
                let git = Arc::clone(&self.engines.git_engine);
                let path_for_delete = path.clone();
                let delete_result = tokio::task::spawn_blocking(move || {
                    git.delete_directory(&path_for_delete)
                })
                .await;
                match delete_result {
                    Ok(Ok(())) => {
                        tracing::info!(session_id = %session_id, "Remote session clone deleted");
                    }
                    Ok(Err(e)) => {
                        tracing::error!(
                            session_id = %session_id,
                            error = %e,
                            "Failed to delete remote session clone"
                        );
                        return; // don't mark as closed
                    }
                    Err(e) => {
                        tracing::error!(
                            session_id = %session_id,
                            error = %e,
                            "Delete directory task panicked"
                        );
                        return;
                    }
                }
            }
        }

        // Mark session as closed.
        let closed_at = chrono::Utc::now().to_rfc3339();
        let _ = self.store.close_session_force(session_id, &closed_at);

        // Remove from in-memory sessions map.
        self.sessions.lock().await.remove(session_id);

        tracing::info!(
            worker_id = %self.worker_id,
            session_id = %session_id,
            "Session closed after drain"
        );
    }
}
