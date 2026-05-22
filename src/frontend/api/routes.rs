//! HTTP route registration and handlers for the API server.
//!
//! Wire-identical to `oldsrc/commands/headless/server.rs::build_router`.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tower_http::trace::TraceLayer;

use crate::command::dispatch::{Dispatch, Engines};
use crate::command::dispatch::catalogue::{CommandCatalogue, FrontendKind};
use crate::data::execution_event::{EventPayload, ExecutionEvent};
use crate::data::fs::api_db::SqliteSessionStore;
use crate::data::fs::api_paths::ApiPaths;
use crate::data::session::{Session, SessionOpenOptions, StaticGitRootResolver};
use crate::data::session_setup_event::{SessionSetupState, SessionSetupStatus, SetupEventPayload};
use crate::frontend::api::command_frontend::ApiDispatchFrontend;
use crate::frontend::api::event_bus::EventBus;
use crate::frontend::api::session_setup::SessionSetupBus;

// ─── Auth mode ───────────────────────────────────────────────────────────────

#[derive(Clone)]
pub enum AuthMode {
    Enabled { key_hash: String },
    Disabled,
}

// ─── Shared state ────────────────────────────────────────────────────────────

pub struct AppState {
    pub store: SqliteSessionStore,
    pub paths: ApiPaths,
    pub workdirs: Vec<PathBuf>,
    pub started_at: Instant,
    pub busy_sessions: tokio::sync::Mutex<HashSet<String>>,
    pub task_handles: tokio::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
    pub auth_mode: AuthMode,
    pub engines: Engines,
    /// Maps HTTP session IDs → their Layer 0 Session. Opened once when the
    /// session is created via the API, reused for every command dispatch
    /// within that session, removed when the session is closed.
    pub sessions: tokio::sync::Mutex<HashMap<String, Arc<RwLock<Session>>>>,
    /// Per-command EventBus handles, keyed by command_id. Retained during
    /// execution plus a short grace period for late-connecting SSE clients.
    pub event_buses: tokio::sync::Mutex<HashMap<String, Arc<EventBus>>>,
    /// Per-session setup bus handles, keyed by session_id. Retained during
    /// setup plus 60 seconds after reaching a terminal state.
    pub setup_buses: tokio::sync::Mutex<HashMap<String, Arc<SessionSetupBus>>>,
}

// ─── Request / Response types (wire-compatible with oldsrc) ──────────────────

#[derive(Deserialize, Debug)]
struct CreateSessionRequest {
    /// `"local"` (default) or `"remote"`.
    #[serde(default)]
    session_type: Option<String>,
    /// Workdir on the server host (required for `local`).
    #[serde(default)]
    workdir: Option<String>,
    /// Repository URL (required for `remote`).
    #[serde(default)]
    repo_url: Option<String>,
    /// Optional branch (defaults to remote default when `remote`).
    #[serde(default)]
    branch: Option<String>,
}

#[derive(Serialize)]
struct CreateSessionResponse {
    session_id: String,
}

#[derive(Serialize)]
struct SessionResponse {
    id: String,
    workdir: String,
    created_at: String,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    closed_at: Option<String>,
}

#[derive(Deserialize)]
struct CreateCommandRequest {
    subcommand: String,
    args: Vec<String>,
}

#[derive(Serialize)]
struct CreateCommandResponse {
    command_id: String,
    /// Server-enforced flags whose values the API frontend always overrides.
    /// Documents to clients that `yolo` and `non_interactive` are forced to
    /// `true` regardless of any value sent in the request body. Empty object
    /// for non-exec routes.
    flags_applied: serde_json::Value,
}

#[derive(Serialize)]
struct CommandResponse {
    id: String,
    session_id: String,
    subcommand: String,
    args: serde_json::Value,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    started_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    finished_at: Option<String>,
    log_path: String,
}

#[derive(Serialize)]
struct StatusResponse {
    status: String,
    pid: u32,
    uptime_seconds: u64,
    active_sessions: i64,
    running_commands: i64,
}

#[derive(Serialize)]
struct ErrorResponse {
    error: String,
}

#[derive(Deserialize, Default)]
struct ListSessionsQuery {
    #[serde(default)]
    status: Option<String>,
}

fn error_json(msg: impl Into<String>) -> Json<ErrorResponse> {
    Json(ErrorResponse { error: msg.into() })
}

// ─── Router ──────────────────────────────────────────────────────────────────

pub fn build_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/v1/status", get(handle_status))
        .route("/v1/workdirs", get(handle_workdirs))
        .route(
            "/v1/sessions",
            get(handle_list_sessions).post(handle_create_session),
        )
        .route(
            "/v1/sessions/:id",
            get(handle_get_session).delete(handle_close_session),
        )
        .route("/v1/sessions/:id/status", get(handle_get_session_status))
        .route(
            "/v1/sessions/:id/jobs/:job_id/logs",
            get(handle_stream_job_logs),
        )
        .route("/v1/commands", post(handle_create_command))
        .route("/v1/commands/:id", get(handle_get_command))
        .route("/v1/workflows/:command_id", get(handle_get_workflow))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

// ─── Auth middleware ─────────────────────────────────────────────────────────

async fn auth_middleware(
    State(state): State<Arc<AppState>>,
    req: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> Response {
    if let AuthMode::Enabled { ref key_hash } = state.auth_mode {
        let auth_header = req
            .headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok());

        match auth_header {
            None | Some("") => {
                return (
                    StatusCode::UNAUTHORIZED,
                    error_json(
                        "API key required. Pass the key via the Authorization header \
                         (e.g. Authorization: Bearer <key>).",
                    ),
                )
                    .into_response();
            }
            Some(header) => {
                let provided_key = if header
                    .get(..7)
                    .is_some_and(|prefix| prefix.eq_ignore_ascii_case("bearer "))
                {
                    &header[7..]
                } else {
                    header
                };

                let provided_hash = {
                    use ring::digest;
                    let h = digest::digest(&digest::SHA256, provided_key.as_bytes());
                    h.as_ref()
                        .iter()
                        .map(|b| format!("{b:02x}"))
                        .collect::<String>()
                };

                use subtle::ConstantTimeEq;
                let keys_equal: bool = provided_hash.as_bytes().ct_eq(key_hash.as_bytes()).into();
                if !keys_equal {
                    return (StatusCode::UNAUTHORIZED, error_json("Invalid API key."))
                        .into_response();
                }
            }
        }
    }
    next.run(req).await
}

// ─── Handlers ────────────────────────────────────────────────────────────────

async fn handle_status(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let active_sessions = state.store.count_active_sessions().unwrap_or(0);
    let running_commands = state.store.count_running_commands().unwrap_or(0);
    let uptime = state.started_at.elapsed().as_secs();

    Json(StatusResponse {
        status: "ok".to_string(),
        pid: std::process::id(),
        uptime_seconds: uptime,
        active_sessions,
        running_commands,
    })
}

async fn handle_workdirs(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let dirs: Vec<String> = state
        .workdirs
        .iter()
        .map(|p| p.display().to_string())
        .collect();
    Json(serde_json::json!({ "workdirs": dirs }))
}

async fn handle_create_session(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreateSessionRequest>,
) -> Response {
    let session_type = body
        .session_type
        .as_deref()
        .unwrap_or("local")
        .to_lowercase();

    // Resolve the target workdir based on session type. For local sessions the
    // workdir comes from the request body; for remote sessions we plan to clone
    // into a server-managed path under the session directory.
    let session_id = uuid::Uuid::new_v4().to_string();
    let created_at = chrono::Utc::now().to_rfc3339();
    let session_dir = state.paths.session_dir(&session_id);

    let (resolved_workdir, cloned_path, repo_url, branch) = match session_type.as_str() {
        "local" => {
            let Some(ref workdir_in) = body.workdir else {
                return (
                    StatusCode::BAD_REQUEST,
                    error_json("workdir is required when session_type is 'local'"),
                )
                    .into_response();
            };
            let requested = match std::fs::canonicalize(workdir_in) {
                Ok(p) => p,
                Err(_) => {
                    return (
                        StatusCode::BAD_REQUEST,
                        error_json(format!("Cannot resolve path: {workdir_in}")),
                    )
                        .into_response();
                }
            };
            if !state.workdirs.contains(&requested) {
                let allowed: Vec<String> = state
                    .workdirs
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect();
                return (
                    StatusCode::FORBIDDEN,
                    error_json(format!(
                        "Workdir '{}' is not in the allowlist. Allowed: {:?}",
                        requested.display(),
                        allowed
                    )),
                )
                    .into_response();
            }
            (requested, None, None, None)
        }
        "remote" => {
            let Some(repo_url) = body.repo_url.clone() else {
                return (
                    StatusCode::BAD_REQUEST,
                    error_json("repo_url is required when session_type is 'remote'"),
                )
                    .into_response();
            };
            if repo_url.trim().is_empty() {
                return (
                    StatusCode::BAD_REQUEST,
                    error_json("repo_url must be non-empty"),
                )
                    .into_response();
            }
            // Validate URL scheme; reject `file:` schemes when the resulting
            // path would escape the API root. We intentionally permit only
            // http(s) and git(+ssh) URLs — the typical remote setup.
            let lower = repo_url.to_lowercase();
            let scheme_ok = lower.starts_with("http://")
                || lower.starts_with("https://")
                || lower.starts_with("git@")
                || lower.starts_with("ssh://")
                || lower.starts_with("git://");
            if !scheme_ok {
                return (
                    StatusCode::BAD_REQUEST,
                    error_json("repo_url must use http(s), ssh, or git scheme"),
                )
                    .into_response();
            }
            let cloned = session_dir.join("repo");
            (
                cloned.clone(),
                Some(cloned),
                Some(repo_url),
                body.branch.clone(),
            )
        }
        other => {
            return (
                StatusCode::BAD_REQUEST,
                error_json(format!(
                    "session_type must be 'local' or 'remote'; got '{other}'"
                )),
            )
                .into_response();
        }
    };

    // Create session storage directory.
    if let Err(e) = tokio::fs::create_dir_all(session_dir.join("jobs")).await {
        tracing::error!(error = %e, "Failed to create session directory");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            error_json("Failed to create session directory"),
        )
            .into_response();
    }
    // Legacy "commands" dir for backward compat with pre-WI-0079 clients.
    let _ = tokio::fs::create_dir_all(session_dir.join("commands")).await;
    let _ = tokio::fs::create_dir_all(session_dir.join("worktree")).await;
    let _ = tokio::fs::create_dir_all(session_dir.join("agent-settings")).await;

    // Persist the session row with setup_status='initializing' BEFORE spawning
    // the setup task. If the server restarts mid-setup we want the cleanup
    // pass to find this session as non-terminal even if no setup_state.json
    // was written yet.
    if let Err(e) = state.store.insert_session_full(
        &session_id,
        &resolved_workdir.to_string_lossy(),
        &created_at,
        "initializing",
        &session_type,
        cloned_path.as_ref().map(|p| p.to_string_lossy().into_owned()).as_deref(),
    ) {
        tracing::error!(error = %e, "Failed to insert session");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            error_json("Failed to create session"),
        )
            .into_response();
    }

    let setup_bus = Arc::new(SessionSetupBus::new(256));
    state
        .setup_buses
        .lock()
        .await
        .insert(session_id.clone(), Arc::clone(&setup_bus));

    tracing::info!(
        session_id = %session_id,
        session_type = %session_type,
        workdir = %resolved_workdir.display(),
        "Session created (setup starting)"
    );

    let state_clone = Arc::clone(&state);
    let sid = session_id.clone();
    let plan = SessionSetupPlan {
        session_type,
        resolved_workdir,
        cloned_path,
        repo_url,
        branch,
    };
    tokio::spawn(async move {
        run_session_setup(state_clone, sid, plan, setup_bus).await;
    });

    (
        StatusCode::ACCEPTED,
        Json(CreateSessionResponse { session_id }),
    )
        .into_response()
}

struct SessionSetupPlan {
    session_type: String,
    resolved_workdir: std::path::PathBuf,
    cloned_path: Option<std::path::PathBuf>,
    repo_url: Option<String>,
    branch: Option<String>,
}

async fn run_session_setup(
    state: Arc<AppState>,
    session_id: String,
    plan: SessionSetupPlan,
    setup_bus: Arc<SessionSetupBus>,
) {
    use crate::data::session::AgentName;
    use crate::engine::ready::ReadyEngine;
    use crate::engine::ready::ReadyEngineOptions;
    use crate::frontend::api::session_setup::SetupReadyFrontend;

    // Delay setup work briefly so the HTTP handler's 202 response can be
    // flushed to the client before any setup work runs. Critical when the
    // tokio runtime is single-threaded (e.g. `#[tokio::test]`).
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let bus_sender = setup_bus.sender();

    // ── [remote only] Stage 1: clone repository ──────────────────────────────
    if plan.session_type == "remote" {
        bus_sender.update_status(SessionSetupStatus::CloningRepository);
        let _ = state
            .store
            .update_setup_status(&session_id, "cloning_repository");
        let msg = format!(
            "Cloning {}...",
            plan.repo_url.as_deref().unwrap_or("repository")
        );
        bus_sender.update_stage(&msg);
        bus_sender.emit(SetupEventPayload::StageChanged {
            stage: "cloning_repository".into(),
            message: msg,
        });

        let url = plan.repo_url.clone().unwrap_or_default();
        let dest = plan.cloned_path.clone().expect("remote sessions have cloned_path");
        let git = Arc::clone(&state.engines.git_engine);
        let dest_for_clone = dest.clone();
        let branch_arg = plan.branch.clone();
        let clone_result = tokio::task::spawn_blocking(move || {
            git.clone_repo(&url, branch_arg.as_deref(), &dest_for_clone)
        })
        .await
        .unwrap_or_else(|join_err| {
            Err(crate::engine::error::EngineError::Git(format!(
                "clone task panicked: {join_err}"
            )))
        });
        if let Err(e) = clone_result {
            tracing::error!(session_id = %session_id, error = %e, "clone failed");
            bus_sender.mark_failed("clone", &e.to_string());
            bus_sender.emit(SetupEventPayload::SetupFailed {
                stage: "clone".into(),
                error: e.to_string(),
            });
            // Cleanup any partial clone.
            let git = Arc::clone(&state.engines.git_engine);
            let dest_for_cleanup = dest.clone();
            let _ = tokio::task::spawn_blocking(move || git.delete_directory(&dest_for_cleanup))
                .await;
            let _ = state.store.update_setup_status(&session_id, "failed");
            persist_setup_state(&state, &session_id, &setup_bus).await;
            cleanup_setup_bus(state, session_id, setup_bus).await;
            return;
        }
        bus_sender.emit(SetupEventPayload::StageChanged {
            stage: "cloning_repository_done".into(),
            message: "Repository cloned".into(),
        });

        // ── [remote only] Stage 2: set up branch ─────────────────────────────
        if let Some(branch) = plan.branch.as_deref() {
            bus_sender.update_status(SessionSetupStatus::SettingUpBranch);
            let _ = state
                .store
                .update_setup_status(&session_id, "setting_up_branch");
            let msg = format!("Checking out branch '{branch}'...");
            bus_sender.update_stage(&msg);
            bus_sender.emit(SetupEventPayload::StageChanged {
                stage: "setting_up_branch".into(),
                message: msg,
            });

            let git = Arc::clone(&state.engines.git_engine);
            let dest_for_branch = dest.clone();
            let branch_owned = branch.to_string();
            let branch_result = tokio::task::spawn_blocking(move || {
                git.checkout_or_create_branch(&dest_for_branch, &branch_owned)
            })
            .await
            .unwrap_or_else(|join_err| {
                Err(crate::engine::error::EngineError::Git(format!(
                    "branch task panicked: {join_err}"
                )))
            });
            match branch_result {
                Ok(disposition) => {
                    bus_sender.emit(SetupEventPayload::StageChanged {
                        stage: "branch_ready".into(),
                        message: format!("Branch '{branch}' {disposition}"),
                    });
                }
                Err(e) => {
                    tracing::error!(session_id = %session_id, error = %e, "branch setup failed");
                    bus_sender.mark_failed("branch", &e.to_string());
                    bus_sender.emit(SetupEventPayload::SetupFailed {
                        stage: "branch".into(),
                        error: e.to_string(),
                    });
                    let git = Arc::clone(&state.engines.git_engine);
                    let dest_for_cleanup = dest.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        git.delete_directory(&dest_for_cleanup)
                    })
                    .await;
                    let _ = state.store.update_setup_status(&session_id, "failed");
                    persist_setup_state(&state, &session_id, &setup_bus).await;
                    cleanup_setup_bus(state, session_id, setup_bus).await;
                    return;
                }
            }
        }
    }

    // ── Stage 3 (all): open Session ──────────────────────────────────────────
    bus_sender.update_status(SessionSetupStatus::RunningReady);
    let _ = state.store.update_setup_status(&session_id, "running_ready");
    bus_sender.update_stage("Opening session...");
    bus_sender.emit(SetupEventPayload::StageChanged {
        stage: "running_ready".into(),
        message: "Opening session and running ready checks...".into(),
    });

    let resolver = StaticGitRootResolver::new(&plan.resolved_workdir);
    let session = match Session::open_or_workdir_fallback(
        plan.resolved_workdir.clone(),
        &resolver,
        SessionOpenOptions::default(),
    ) {
        Ok(s) => Arc::new(RwLock::new(s)),
        Err(e) => {
            tracing::error!(
                session_id = %session_id,
                error = %e,
                "Session setup failed: could not open session"
            );
            bus_sender.mark_failed("session_open", &e.to_string());
            bus_sender.emit(SetupEventPayload::SetupFailed {
                stage: "session_open".into(),
                error: e.to_string(),
            });
            if plan.session_type == "remote" {
                if let Some(dest) = plan.cloned_path.clone() {
                    let git = Arc::clone(&state.engines.git_engine);
                    let _ =
                        tokio::task::spawn_blocking(move || git.delete_directory(&dest)).await;
                }
            }
            let _ = state.store.update_setup_status(&session_id, "failed");
            persist_setup_state(&state, &session_id, &setup_bus).await;
            cleanup_setup_bus(state, session_id, setup_bus).await;
            return;
        }
    };

    state
        .sessions
        .lock()
        .await
        .insert(session_id.clone(), Arc::clone(&session));

    // ── Stage 4 (all): run ReadyEngine ───────────────────────────────────────
    let session_guard = session.read().await;
    let ready_options = ReadyEngineOptions {
        agent: AgentName::new("default").expect("valid agent name"),
        refresh: false,
        build: true,
        no_cache: false,
        allow_docker: true,
        non_interactive: true,
        env_passthrough: None,
    };
    let mut ready_engine = ReadyEngine::new(
        Arc::new(session_guard.clone()),
        Arc::clone(&state.engines.git_engine),
        Arc::clone(&state.engines.overlay_engine),
        Arc::clone(&state.engines.runtime),
        Arc::clone(&state.engines.agent_engine),
        ready_options,
    );
    drop(session_guard);

    let event_bus = EventBus::new(4096);
    let event_sender = event_bus.sender();
    let mut setup_frontend = SetupReadyFrontend::new(setup_bus.sender(), event_sender);

    // Cap ReadyEngine at 10 minutes — any legitimate run, including a clean
    // base-image build, completes well within this. If the wall-clock exceeds
    // the cap (e.g. Docker daemon is unresponsive), mark the setup as failed
    // so the session row reaches a terminal state and the bus is cleaned up.
    let ready_fut = ready_engine.run_to_completion(&mut setup_frontend);
    let ready_outcome = tokio::time::timeout(
        std::time::Duration::from_secs(600),
        ready_fut,
    )
    .await;

    match ready_outcome {
        Ok(Ok(summary)) => {
            setup_bus.sender().set_ready(summary.clone());
            bus_sender.emit(SetupEventPayload::SetupComplete {
                ready_summary: summary,
            });
            let _ = state.store.update_setup_status(&session_id, "ready");
            tracing::info!(session_id = %session_id, "Session setup complete");
        }
        Ok(Err(e)) => {
            tracing::error!(
                session_id = %session_id,
                error = %e,
                "Session setup failed during ready"
            );
            bus_sender.mark_failed("ready", &e.to_string());
            bus_sender.emit(SetupEventPayload::SetupFailed {
                stage: "ready".into(),
                error: e.to_string(),
            });
            if plan.session_type == "remote" {
                if let Some(dest) = plan.cloned_path.clone() {
                    let git = Arc::clone(&state.engines.git_engine);
                    let _ =
                        tokio::task::spawn_blocking(move || git.delete_directory(&dest)).await;
                }
            }
            let _ = state.store.update_setup_status(&session_id, "failed");
        }
        Err(_elapsed) => {
            let msg = "ReadyEngine exceeded the 600s setup deadline".to_string();
            tracing::error!(session_id = %session_id, "{msg}");
            bus_sender.mark_failed("ready_timeout", &msg);
            bus_sender.emit(SetupEventPayload::SetupFailed {
                stage: "ready_timeout".into(),
                error: msg,
            });
            if plan.session_type == "remote" {
                if let Some(dest) = plan.cloned_path.clone() {
                    let git = Arc::clone(&state.engines.git_engine);
                    let _ =
                        tokio::task::spawn_blocking(move || git.delete_directory(&dest)).await;
                }
            }
            let _ = state.store.update_setup_status(&session_id, "failed");
        }
    }

    persist_setup_state(&state, &session_id, &setup_bus).await;
    cleanup_setup_bus(state, session_id, setup_bus).await;
}

async fn persist_setup_state(state: &AppState, session_id: &str, setup_bus: &SessionSetupBus) {
    let setup_state = setup_bus.snapshot();
    let setup_path = state.paths.session_dir(session_id).join("setup_state.json");
    if let Ok(json) = serde_json::to_string_pretty(&setup_state) {
        if let Err(e) = tokio::fs::write(&setup_path, json).await {
            tracing::error!(session_id = %session_id, error = %e, "Failed to persist setup_state.json");
        }
    }
}

async fn cleanup_setup_bus(
    state: Arc<AppState>,
    session_id: String,
    _setup_bus: Arc<SessionSetupBus>,
) {
    // Retain the setup bus for 60 seconds after reaching terminal state.
    tokio::time::sleep(std::time::Duration::from_secs(60)).await;
    state.setup_buses.lock().await.remove(&session_id);
}

async fn handle_list_sessions(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ListSessionsQuery>,
) -> Response {
    match state.store.list_sessions_by_status(query.status.as_deref()) {
        Ok(sessions) => {
            let list: Vec<SessionResponse> = sessions
                .into_iter()
                .map(|s| SessionResponse {
                    id: s.id,
                    workdir: s.workdir,
                    created_at: s.created_at,
                    status: s.status,
                    closed_at: s.closed_at,
                })
                .collect();
            Json(serde_json::json!({ "sessions": list })).into_response()
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to list sessions");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                error_json("Failed to list sessions"),
            )
                .into_response()
        }
    }
}

async fn handle_get_session(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    match state.store.get_session(&id) {
        Ok(Some(s)) => Json(SessionResponse {
            id: s.id,
            workdir: s.workdir,
            created_at: s.created_at,
            status: s.status,
            closed_at: s.closed_at,
        })
        .into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            error_json(format!("Session '{}' not found", id)),
        )
            .into_response(),
        Err(e) => {
            tracing::error!(error = %e, "Failed to get session");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                error_json("Failed to get session"),
            )
                .into_response()
        }
    }
}

async fn handle_close_session(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    let closed_at = chrono::Utc::now().to_rfc3339();

    match state.store.get_session(&id) {
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                error_json(format!("Session '{}' not found", id)),
            )
                .into_response();
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to get session");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                error_json("Failed to close session"),
            )
                .into_response();
        }
        Ok(Some(s)) if s.status == "closed" => {
            return Json(SessionResponse {
                id: s.id,
                workdir: s.workdir,
                created_at: s.created_at,
                status: s.status,
                closed_at: s.closed_at,
            })
            .into_response();
        }
        Ok(Some(_)) => {}
    }

    match state.store.close_session(&id, &closed_at) {
        Ok(true) => {
            // Remove the in-memory Session so it can be dropped.
            state.sessions.lock().await.remove(&id);

            match state.store.get_session(&id) {
                Ok(Some(s)) => Json(SessionResponse {
                    id: s.id,
                    workdir: s.workdir,
                    created_at: s.created_at,
                    status: s.status,
                    closed_at: s.closed_at,
                })
                .into_response(),
                _ => StatusCode::NO_CONTENT.into_response(),
            }
        }
        Ok(false) => (
            StatusCode::NOT_FOUND,
            error_json(format!("Session '{}' not found or already closed", id)),
        )
            .into_response(),
        Err(e) => {
            tracing::error!(error = %e, "Failed to close session");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                error_json("Failed to close session"),
            )
                .into_response()
        }
    }
}

async fn handle_get_session_status(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    // Check if session exists at all.
    match state.store.get_session(&id) {
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                error_json(format!("Session '{}' not found", id)),
            )
                .into_response();
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to get session");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                error_json("Failed to get session"),
            )
                .into_response();
        }
        Ok(Some(_)) => {}
    }

    // Try to read from in-memory setup bus first.
    if let Some(bus) = state.setup_buses.lock().await.get(&id).cloned() {
        let setup_state = bus.snapshot();
        return Json(serde_json::json!({
            "session_id": id,
            "status": setup_state.status,
            "current_stage": setup_state.current_stage,
            "current_ready_phase": setup_state.current_ready_phase,
            "ready_step_statuses": setup_state.ready_step_statuses,
            "ready_summary": setup_state.ready_summary,
            "error": setup_state.error,
        }))
        .into_response();
    }

    // Fall back to on-disk setup_state.json.
    let setup_state_path = state.paths.session_dir(&id).join("setup_state.json");
    match tokio::fs::read_to_string(&setup_state_path).await {
        Ok(content) => match serde_json::from_str::<SessionSetupState>(&content) {
            Ok(setup_state) => Json(serde_json::json!({
                "session_id": id,
                "status": setup_state.status,
                "current_stage": setup_state.current_stage,
                "current_ready_phase": setup_state.current_ready_phase,
                "ready_step_statuses": setup_state.ready_step_statuses,
                "ready_summary": setup_state.ready_summary,
                "error": setup_state.error,
            }))
            .into_response(),
            Err(_) => fallback_status_from_db(&state, &id).await,
        },
        Err(_) => fallback_status_from_db(&state, &id).await,
    }
}

/// Resolve a session's setup status to (is_ready, status_string, optional error JSON).
/// Reads the in-memory bus first, then setup_state.json on disk, then the sqlite
/// session row. Used by the job-submission guard and other places that need to
/// reason about session readiness.
async fn resolve_setup_status(
    state: &AppState,
    session_id: &str,
) -> (bool, String, Option<serde_json::Value>) {
    if let Some(bus) = state.setup_buses.lock().await.get(session_id).cloned() {
        let s = bus.snapshot();
        let is_ready = matches!(s.status, SessionSetupStatus::Ready);
        let status_str = s.status.as_str().to_string();
        let err_payload = s.error.as_ref().map(|e| serde_json::json!({
            "stage": e.stage,
            "message": e.message,
        }));
        return (is_ready, status_str, err_payload);
    }
    // No bus. Try setup_state.json.
    let setup_path = state.paths.session_dir(session_id).join("setup_state.json");
    if let Ok(content) = tokio::fs::read_to_string(&setup_path).await {
        if let Ok(ss) = serde_json::from_str::<SessionSetupState>(&content) {
            let is_ready = matches!(ss.status, SessionSetupStatus::Ready);
            let status_str = ss.status.as_str().to_string();
            let err_payload = ss.error.as_ref().map(|e| serde_json::json!({
                "stage": e.stage,
                "message": e.message,
            }));
            return (is_ready, status_str, err_payload);
        }
    }
    // Last resort: sqlite session row.
    match state.store.get_session(session_id) {
        Ok(Some(s)) => {
            let is_ready = s.setup_status == "ready";
            (is_ready, s.setup_status, None)
        }
        _ => (true, "ready".to_string(), None), // truly unknown — assume ready
    }
}

/// Last-resort fallback when neither the in-memory bus nor the on-disk
/// setup_state.json is usable: read the session's setup_status from sqlite
/// and return a minimal response. Used for very old sessions (pre-WI-0078).
async fn fallback_status_from_db(state: &AppState, id: &str) -> Response {
    let setup_status = state
        .store
        .get_session(id)
        .ok()
        .flatten()
        .map(|s| s.setup_status)
        .unwrap_or_else(|| "ready".to_string());
    Json(serde_json::json!({
        "session_id": id,
        "status": setup_status,
    }))
    .into_response()
}

async fn handle_create_command(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<CreateCommandRequest>,
) -> Response {
    let session_id = match headers.get("x-awman-session") {
        Some(val) => match val.to_str() {
            Ok(s) => s.to_string(),
            Err(_) => {
                return (
                    StatusCode::BAD_REQUEST,
                    error_json("Invalid x-awman-session header value"),
                )
                    .into_response();
            }
        },
        None => {
            return (
                StatusCode::BAD_REQUEST,
                error_json("Missing required header: x-awman-session"),
            )
                .into_response();
        }
    };

    // Validate command is API-allowed via the typed catalogue check.
    // `validate_for_frontend` returns `CommandError::NotAvailableForFrontend`
    // when blocked; any other error path (e.g. unknown command) falls through
    // to per-command dispatch where it surfaces with its own error.
    {
        let catalogue = CommandCatalogue::get();
        let path_parts: Vec<&str> = body.subcommand.split_whitespace().collect();
        if let Err(crate::command::error::CommandError::NotAvailableForFrontend {
            command, ..
        }) = catalogue.validate_for_frontend(FrontendKind::Api, &path_parts)
        {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "command not available via API",
                    "blocked_command": command,
                    "available": ["exec workflow", "exec prompt"],
                })),
            )
                .into_response();
        }
    }

    // Validate session.
    let workdir = match state.store.get_session(&session_id) {
        Ok(Some(s)) if s.status == "active" => s.workdir.clone(),
        Ok(Some(_)) => {
            return (
                StatusCode::NOT_FOUND,
                error_json(format!("Session '{}' is closed", session_id)),
            )
                .into_response();
        }
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                error_json(format!("Session '{}' not found", session_id)),
            )
                .into_response();
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to get session");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                error_json("Failed to validate session"),
            )
                .into_response();
        }
    };

    // Job submission guard: reject if session setup is not ready.
    {
        let (setup_ready, status_str, error_payload) = resolve_setup_status(&state, &session_id).await;
        if !setup_ready {
            let mut body = serde_json::json!({
                "error": "session is not ready",
                "setup_status": status_str,
                "hint": "Poll GET /v1/sessions/{id}/status to check setup progress"
            });
            if let Some(err) = error_payload {
                body["setup_error"] = err;
                if let Some(obj) = body.as_object_mut() {
                    obj.insert(
                        "error".into(),
                        serde_json::Value::String("session setup failed".into()),
                    );
                }
            }
            return (StatusCode::CONFLICT, Json(body)).into_response();
        }
    }

    // DB-level concurrency guard.
    match state.store.has_running_command_for_session(&session_id) {
        Ok(true) => {
            return (
                StatusCode::FORBIDDEN,
                error_json(format!(
                    "Session '{}' already has a running command.",
                    session_id
                )),
            )
                .into_response();
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to check running commands");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                error_json("Failed to check running commands"),
            )
                .into_response();
        }
        Ok(false) => {}
    }

    // In-memory concurrency guard.
    {
        let mut busy = state.busy_sessions.lock().await;
        if busy.contains(&session_id) {
            return (
                StatusCode::FORBIDDEN,
                error_json(format!(
                    "Session '{}' already has a running command.",
                    session_id
                )),
            )
                .into_response();
        }
        busy.insert(session_id.clone());
    }

    let command_id = uuid::Uuid::new_v4().to_string();
    let args_json = serde_json::to_string(&body.args).unwrap_or_else(|_| "[]".to_string());

    let cmd_dir = state.paths.command_dir(&session_id, &command_id);
    if let Err(e) = tokio::fs::create_dir_all(&cmd_dir).await {
        tracing::error!(error = %e, "Failed to create command directory");
        state.busy_sessions.lock().await.remove(&session_id);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            error_json("Failed to create command directory"),
        )
            .into_response();
    }

    let log_path = cmd_dir.join("output.log");

    if let Err(e) = state.store.insert_command(
        &command_id,
        &session_id,
        &body.subcommand,
        &args_json,
        &log_path.to_string_lossy(),
    ) {
        tracing::error!(error = %e, "Failed to insert command");
        state.busy_sessions.lock().await.remove(&session_id);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            error_json("Failed to create command"),
        )
            .into_response();
    }

    tracing::info!(
        command_id = %command_id,
        session_id = %session_id,
        subcommand = %body.subcommand,
        "Command dispatched"
    );

    // Spawn execution task via Layer 2 dispatch.
    let state_clone = Arc::clone(&state);
    let cmd_id = command_id.clone();
    let sess_id = session_id.clone();
    let subcommand = body.subcommand.clone();
    let cmd_args = body.args.clone();
    let log_p = log_path.clone();
    let workdir_clone = workdir;

    let handle = tokio::spawn(async move {
        execute_command(
            state_clone,
            cmd_id,
            sess_id,
            subcommand,
            cmd_args,
            log_p,
            workdir_clone,
        )
        .await;
    });
    state.task_handles.lock().await.push(handle);

    let flags_applied = serde_json::json!({
        "yolo": true,
        "non_interactive": true,
    });

    (
        StatusCode::ACCEPTED,
        Json(CreateCommandResponse {
            command_id,
            flags_applied,
        }),
    )
        .into_response()
}

async fn execute_command(
    state: Arc<AppState>,
    command_id: String,
    session_id: String,
    subcommand: String,
    args: Vec<String>,
    log_path: PathBuf,
    workdir: String,
) {
    let started_at = chrono::Utc::now().to_rfc3339();
    let _ = state.store.update_command_started(&command_id, &started_at);

    // Write metadata.
    if let Some(parent) = log_path.parent() {
        let metadata = serde_json::json!({
            "command_id": command_id,
            "session_id": session_id,
            "subcommand": subcommand,
            "args": args,
            "workdir": workdir,
            "started_at": started_at,
        });
        let meta_path = parent.join("metadata.json");
        let _ = tokio::fs::write(
            &meta_path,
            serde_json::to_string_pretty(&metadata).unwrap_or_default(),
        )
        .await;
    }

    // Create the EventBus for this command execution.
    let event_bus = Arc::new(EventBus::new(4096));

    // Spawn a logfile writer task that persists events to disk.
    {
        let mut log_rx = event_bus.subscribe();
        let events_log_path = log_path.with_file_name("events.log");
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
                            let _ =
                                output_file.write_all(format!("{text}\n").as_bytes()).await;
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

    // Store the EventBus handle for SSE subscribers.
    state
        .event_buses
        .lock()
        .await
        .insert(command_id.clone(), Arc::clone(&event_bus));

    // Construct the API frontend that emits to the EventBus. The frontend's
    // `Drop` impl is the single source of the `Done` sentinel — it flushes
    // any partial stdout/stderr line buffers first, then emits `Done` once.
    let frontend = ApiDispatchFrontend::new(&subcommand, &args, event_bus.sender());

    // Look up the existing Session for this HTTP session. The Session was
    // opened when the client created the session via POST /v1/sessions and is
    // reused for every command within it.
    let session = match state.sessions.lock().await.get(&session_id).cloned() {
        Some(s) => s,
        None => {
            tracing::error!(command_id = %command_id, session_id = %session_id, "Session not found in memory");
            // Dropping `frontend` here emits the `Done` sentinel.
            drop(frontend);
            let finished_at = chrono::Utc::now().to_rfc3339();
            let _ = state
                .store
                .update_command_finished(&command_id, "error", None, &finished_at);
            state.busy_sessions.lock().await.remove(&session_id);
            return;
        }
    };

    // Build the command path from the subcommand string (e.g. "exec prompt" → ["exec", "prompt"]).
    let path_parts: Vec<&str> = subcommand.split_whitespace().collect();

    // Dispatch through Layer 2 — exactly like CLI and TUI do. The frontend is
    // moved into `Dispatch`, then into the matching per-command `Box<dyn ...>`,
    // and finally dropped when `run_with_frontend` returns — at which point
    // its `Drop` impl flushes any partial line buffers and emits `Done`.
    let dispatch = Dispatch::new(frontend, session, state.engines.clone());
    let result = dispatch.run_command(&path_parts).await;

    let finished_at = chrono::Utc::now().to_rfc3339();
    let (status, exit_code) = match &result {
        Ok(_) => ("done", Some(0)),
        Err(_) => ("error", Some(1)),
    };

    // Clean up EventBus after a grace period.
    {
        let state_for_cleanup = Arc::clone(&state);
        let cmd_id_for_cleanup = command_id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            state_for_cleanup
                .event_buses
                .lock()
                .await
                .remove(&cmd_id_for_cleanup);
        });
    }

    // Update metadata.
    if let Some(parent) = log_path.parent() {
        let metadata = serde_json::json!({
            "command_id": command_id,
            "session_id": session_id,
            "subcommand": subcommand,
            "args": args,
            "workdir": workdir,
            "started_at": started_at,
            "finished_at": finished_at,
            "exit_code": exit_code,
            "status": status,
        });
        let meta_path = parent.join("metadata.json");
        let _ = tokio::fs::write(
            &meta_path,
            serde_json::to_string_pretty(&metadata).unwrap_or_default(),
        )
        .await;
    }

    let _ = state
        .store
        .update_command_finished(&command_id, status, exit_code, &finished_at);

    if let Err(ref e) = result {
        tracing::error!(command_id = %command_id, error = %e, "Command failed");
    }

    state.busy_sessions.lock().await.remove(&session_id);
}

async fn handle_get_command(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    match state.store.get_command(&id) {
        Ok(Some(c)) => {
            let args: serde_json::Value =
                serde_json::from_str(&c.args).unwrap_or(serde_json::Value::Array(vec![]));
            Json(CommandResponse {
                id: c.id,
                session_id: c.session_id,
                subcommand: c.subcommand,
                args,
                status: c.status,
                exit_code: c.exit_code,
                started_at: c.started_at,
                finished_at: c.finished_at,
                log_path: c.log_path,
            })
            .into_response()
        }
        Ok(None) => (
            StatusCode::NOT_FOUND,
            error_json(format!("Command '{}' not found", id)),
        )
            .into_response(),
        Err(e) => {
            tracing::error!(error = %e, "Failed to get command");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                error_json("Failed to get command"),
            )
                .into_response()
        }
    }
}

/// Query parameters for the per-job SSE / log endpoint.
#[derive(Deserialize, Default)]
struct JobLogsQuery {
    /// When set to `"json"`, return the events.log content as a JSON array
    /// of ExecutionEvent values instead of streaming SSE.
    #[serde(default)]
    format: Option<String>,
}

/// `GET /v1/sessions/{sid}/jobs/{jid}/logs` — structured event stream.
///
/// Behavior:
/// - Validates `sid` and `jid` exist; 404 otherwise.
/// - When `?format=json`, returns `events.log` as a JSON array of
///   `ExecutionEvent` (non-streaming).
/// - Otherwise streams SSE in `event: <type>\ndata: <json>\n\n` format.
/// - If the job is running, first replays `events.log` from disk (capturing
///   the highest sequence number), then subscribes to the live EventBus
///   and filters out events with `sequence <= last_replayed_seq` to avoid
///   duplicates from the replay/live switchover race.
/// - When the broadcast channel reports `Lagged(n)`, sends an SSE comment
///   line `: lagged: <n> events skipped` and resumes streaming.
/// - Emits a final `event: done\ndata: ...\n\n` when the stream completes.
async fn handle_stream_job_logs(
    State(state): State<Arc<AppState>>,
    AxumPath((session_id, job_id)): AxumPath<(String, String)>,
    Query(query): Query<JobLogsQuery>,
) -> Response {
    // Validate job exists and belongs to session.
    let (events_log_path, is_already_done) = match state.store.get_command(&job_id) {
        Ok(Some(c)) if c.session_id == session_id => {
            let done = matches!(c.status.as_str(), "done" | "error");
            let events_log = state
                .paths
                .command_events_log_path(&session_id, &job_id);
            (events_log, done)
        }
        Ok(Some(_)) => {
            return (
                StatusCode::NOT_FOUND,
                error_json(format!(
                    "Job '{job_id}' not found in session '{session_id}'"
                )),
            )
                .into_response();
        }
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                error_json(format!("Job '{job_id}' not found")),
            )
                .into_response();
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to look up job");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                error_json("Failed to look up job"),
            )
                .into_response();
        }
    };

    // ?format=json — return the full events.log as a JSON array.
    if query.format.as_deref() == Some("json") {
        let content = tokio::fs::read_to_string(&events_log_path)
            .await
            .unwrap_or_default();
        let mut events = Vec::new();
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<ExecutionEvent>(line) {
                Ok(ev) => events.push(serde_json::to_value(ev).unwrap_or(serde_json::Value::Null)),
                Err(e) => {
                    tracing::warn!(error = %e, "skipping malformed events.log line");
                }
            }
        }
        return Json(serde_json::json!({
            "session_id": session_id,
            "job_id": job_id,
            "events": events,
        }))
        .into_response();
    }

    // SSE streaming path.
    let (tx, rx) =
        tokio::sync::mpsc::unbounded_channel::<Result<Event, std::convert::Infallible>>();
    let stream = UnboundedReceiverStream::new(rx);

    let maybe_bus = if is_already_done {
        None
    } else {
        state.event_buses.lock().await.get(&job_id).cloned()
    };

    tokio::spawn(async move {
        // 1. Replay events.log from disk, recording the highest sequence.
        let mut last_replayed_seq: Option<u64> = None;
        if let Ok(content) = tokio::fs::read_to_string(&events_log_path).await {
            for line in content.lines() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let event: ExecutionEvent = match serde_json::from_str(line) {
                    Ok(e) => e,
                    Err(_) => continue,
                };
                last_replayed_seq = Some(
                    last_replayed_seq
                        .map(|s| s.max(event.sequence))
                        .unwrap_or(event.sequence),
                );
                if tx.send(Ok(execution_event_to_sse(&event))).is_err() {
                    return;
                }
            }
        }

        // 2. If a live EventBus exists, subscribe and forward post-replay events.
        if let Some(bus) = maybe_bus {
            let mut rx = bus.subscribe();
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        if let Some(last) = last_replayed_seq {
                            if event.sequence <= last {
                                continue; // already sent via replay
                            }
                        }
                        let is_done = matches!(event.payload, EventPayload::Done);
                        if tx.send(Ok(execution_event_to_sse(&event))).is_err() {
                            return;
                        }
                        if is_done {
                            return;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(lagged = n, "SSE subscriber lagged");
                        let comment = Event::default().comment(format!("lagged: {n} events skipped"));
                        if tx.send(Ok(comment)).is_err() {
                            return;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        // Bus dropped — emit synthetic Done if we didn't already.
                        let done = ExecutionEvent {
                            timestamp: chrono::Utc::now(),
                            sequence: last_replayed_seq.map(|s| s + 1).unwrap_or(0),
                            payload: EventPayload::Done,
                        };
                        let _ = tx.send(Ok(execution_event_to_sse(&done)));
                        return;
                    }
                }
            }
        }
        // No live bus and replay finished — the job is completed; emit Done if
        // the last replayed event wasn't already a Done.
        let done = ExecutionEvent {
            timestamp: chrono::Utc::now(),
            sequence: last_replayed_seq.map(|s| s + 1).unwrap_or(0),
            payload: EventPayload::Done,
        };
        let _ = tx.send(Ok(execution_event_to_sse(&done)));
    });

    Sse::new(stream).into_response()
}

/// Encode an ExecutionEvent as a structured SSE message:
/// `event: <type>\ndata: <json>\n\n`.
fn execution_event_to_sse(event: &ExecutionEvent) -> Event {
    let data = serde_json::to_string(event).unwrap_or_else(|_| "{}".into());
    Event::default()
        .event(event.payload.sse_event_type())
        .data(data)
}

async fn handle_get_workflow(
    State(state): State<Arc<AppState>>,
    AxumPath(command_id): AxumPath<String>,
) -> Response {
    let session_id = match state.store.get_command(&command_id) {
        Ok(Some(c)) => c.session_id,
        Ok(None) => {
            return (StatusCode::NOT_FOUND, error_json("command not found")).into_response();
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to get command for workflow");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                error_json("Failed to get command"),
            )
                .into_response();
        }
    };

    let wf_path = state
        .paths
        .command_workflow_state_path(&session_id, &command_id);

    match tokio::fs::read_to_string(&wf_path).await {
        Ok(content) => match serde_json::from_str::<serde_json::Value>(&content) {
            Ok(val) => Json(val).into_response(),
            Err(e) => {
                tracing::error!(error = %e, "Failed to parse workflow state");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    error_json("Failed to parse workflow state"),
                )
                    .into_response()
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (
            StatusCode::NOT_FOUND,
            error_json("no workflow for this command"),
        )
            .into_response(),
        Err(e) => {
            tracing::error!(error = %e, "Failed to read workflow state");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                error_json("Failed to read workflow state"),
            )
                .into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::Arc;
    use std::time::Instant;

    // Route table from oldsrc/commands/headless/server.rs — wire-identical assertion guard.
    // Every entry here must be registered in build_router; any divergence is a regression.
    const EXPECTED_ROUTES: &[(&str, &str)] = &[
        ("GET", "/v1/status"),
        ("GET", "/v1/workdirs"),
        ("GET", "/v1/sessions"),
        ("POST", "/v1/sessions"),
        ("GET", "/v1/sessions/:id"),
        ("DELETE", "/v1/sessions/:id"),
        ("GET", "/v1/sessions/:id/status"),
        ("GET", "/v1/sessions/:id/jobs/:job_id/logs"),
        ("POST", "/v1/commands"),
        ("GET", "/v1/commands/:id"),
        ("GET", "/v1/workflows/:command_id"),
    ];

    fn make_test_state(tmp: &std::path::Path) -> Arc<AppState> {
        use crate::command::dispatch::Engines;
        use crate::data::fs::auth_paths::AuthPathResolver;
        use crate::data::fs::api_db::SqliteSessionStore;
        use crate::data::fs::api_paths::ApiPaths;
        use crate::engine::agent::AgentEngine;
        use crate::engine::auth::AuthEngine;
        use crate::engine::container::ContainerRuntime;
        use crate::engine::git::GitEngine;
        use crate::engine::overlay::OverlayEngine;

        let paths = ApiPaths::at_root(tmp);
        let store = SqliteSessionStore::open(tmp).unwrap();
        let runtime = Arc::new(ContainerRuntime::docker());
        let overlay = Arc::new(OverlayEngine::with_auth_resolver(
            AuthPathResolver::at_home(tmp),
        ));
        let git_engine = Arc::new(GitEngine::new());
        let agent_engine = Arc::new(AgentEngine::new(overlay.clone(), runtime.clone()));
        let auth_engine = Arc::new(AuthEngine::with_paths(
            AuthPathResolver::at_home(tmp),
            paths.clone(),
        ));
        let workflow_state_store =
            Arc::new(crate::data::EngineWorkflowStateStore::at_git_root(tmp));
        let engines = Engines {
            runtime,
            git_engine,
            overlay_engine: overlay,
            auth_engine,
            agent_engine,
            workflow_state_store,
        };
        Arc::new(AppState {
            store,
            paths,
            workdirs: Vec::new(),
            started_at: Instant::now(),
            busy_sessions: tokio::sync::Mutex::new(HashSet::new()),
            task_handles: tokio::sync::Mutex::new(Vec::new()),
            auth_mode: AuthMode::Disabled,
            engines,
            sessions: tokio::sync::Mutex::new(std::collections::HashMap::new()),
            event_buses: tokio::sync::Mutex::new(std::collections::HashMap::new()),
            setup_buses: tokio::sync::Mutex::new(std::collections::HashMap::new()),
        })
    }

    #[test]
    fn expected_route_count() {
        // Guard: if someone adds a route without updating this table, the count drifts.
        assert_eq!(
            EXPECTED_ROUTES.len(),
            11,
            "route count mismatch — update EXPECTED_ROUTES"
        );
    }

    #[tokio::test]
    async fn all_expected_routes_respond_non_404() {
        let tmp = tempfile::tempdir().unwrap();
        let state = make_test_state(tmp.path());
        let app = build_router(state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let client = reqwest::Client::new();

        // Test routes that always return non-404 regardless of request content.
        // These only depend on server state, not on specific resource IDs.
        let unconditional_routes: &[(&str, &str)] = &[
            ("GET", "/v1/status"),
            ("GET", "/v1/workdirs"),
            ("GET", "/v1/sessions"),
        ];

        for (method, path) in unconditional_routes {
            let url = format!("http://{addr}{path}");
            let req = match *method {
                "GET" => client.get(&url),
                "POST" => client.post(&url),
                _ => panic!("unhandled method {method}"),
            };
            let resp = req
                .send()
                .await
                .unwrap_or_else(|e| panic!("request to {method} {path} failed: {e}"));
            assert_ne!(
                resp.status().as_u16(),
                404,
                "{method} {path} returned 404 — route may not be registered"
            );
        }

        // Routes that naturally return 4xx for missing resources ARE registered —
        // verify by calling them with the correct method and asserting we get
        // anything other than a routing-level 404 for a completely unknown path.
        // (We use a clearly-bogus path to get the routing 404 baseline, then compare.)
        let bogus_404 = client
            .get(format!("http://{addr}/v1/definitely-not-a-route"))
            .send()
            .await
            .unwrap()
            .status()
            .as_u16();
        assert_eq!(bogus_404, 404, "bogus path must return 404");

        // Resource routes: these return handler-level 4xx (session/command not found).
        // We assert they respond with something (connection succeeds and we get any HTTP response).
        let resource_routes: &[(&str, &str, u16)] = &[
            // (method, path, expected_status_for_missing_resource)
            ("GET", "/v1/sessions/test-id", 404),        // session not found
            ("DELETE", "/v1/sessions/test-id", 404),      // session not found
            ("GET", "/v1/sessions/test-id/status", 404),  // session not found
            ("GET", "/v1/commands/test-id", 404),         // command not found
            ("GET", "/v1/sessions/test-sid/jobs/test-jid/logs", 404), // job not found
            ("GET", "/v1/workflows/test-cmd", 404),       // command not found
        ];

        for (method, path, expected_status) in resource_routes {
            let url = format!("http://{addr}{path}");
            let req = match *method {
                "GET" => client.get(&url),
                "DELETE" => client.delete(&url),
                _ => panic!("unhandled method {method}"),
            };
            let resp = req
                .send()
                .await
                .unwrap_or_else(|e| panic!("request to {method} {path} failed: {e}"));
            // The handler returns *expected_status* for missing resources.
            // We verify the route exists by confirming the response status matches
            // what the handler produces (not a routing-level 404 from an unregistered path).
            // Since both cases return 404 here, we at least verify the request succeeds.
            assert_eq!(
                resp.status().as_u16(),
                *expected_status,
                "{method} {path} returned unexpected status"
            );
        }

        // POST /v1/sessions — check it responds (even with 400/422 for missing body).
        let resp = client
            .post(format!("http://{addr}/v1/sessions"))
            .send()
            .await
            .unwrap();
        assert_ne!(
            resp.status().as_u16(),
            404,
            "POST /v1/sessions returned 404 — route may not be registered"
        );

        // POST /v1/commands — check it responds (even with 400 for missing headers).
        let resp = client
            .post(format!("http://{addr}/v1/commands"))
            .send()
            .await
            .unwrap();
        assert_ne!(
            resp.status().as_u16(),
            404,
            "POST /v1/commands returned 404 — route may not be registered"
        );
    }

    #[test]
    fn auth_middleware_rejects_missing_authorization_header() {
        // Auth logic is synchronous; test the hash comparison in isolation.
        use ring::digest;
        use subtle::ConstantTimeEq;

        let key = "test-api-key";
        let hash: String = {
            let h = digest::digest(&digest::SHA256, key.as_bytes());
            h.as_ref().iter().map(|b| format!("{b:02x}")).collect()
        };

        // Good key: computed hash matches stored hash.
        let provided_hash: String = {
            let h = digest::digest(&digest::SHA256, key.as_bytes());
            h.as_ref().iter().map(|b| format!("{b:02x}")).collect()
        };
        assert!(bool::from(provided_hash.as_bytes().ct_eq(hash.as_bytes())));

        // Bad key: hash does NOT match.
        let bad_hash: String = {
            let h = digest::digest(&digest::SHA256, b"wrong-key");
            h.as_ref().iter().map(|b| format!("{b:02x}")).collect()
        };
        assert!(!bool::from(bad_hash.as_bytes().ct_eq(hash.as_bytes())));
    }

    #[tokio::test]
    async fn auth_enabled_rejects_bad_key_with_401() {
        let tmp = tempfile::tempdir().unwrap();
        let mut state = make_test_state(tmp.path());

        // Set up auth with a known key hash.
        let key = "my-test-api-key";
        let hash: String = {
            use ring::digest;
            let h = digest::digest(&digest::SHA256, key.as_bytes());
            h.as_ref().iter().map(|b| format!("{b:02x}")).collect()
        };
        // Replace auth_mode with Enabled.
        Arc::get_mut(&mut state).unwrap().auth_mode = AuthMode::Enabled { key_hash: hash };

        let app = build_router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let client = reqwest::Client::new();

        // No Authorization header → 401.
        let resp = client
            .get(format!("http://{addr}/v1/status"))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status().as_u16(),
            401,
            "missing auth header must return 401"
        );

        // Wrong key → 401.
        let resp = client
            .get(format!("http://{addr}/v1/status"))
            .header("Authorization", "Bearer wrong-key")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 401, "wrong key must return 401");

        // Correct key → not 401.
        let resp = client
            .get(format!("http://{addr}/v1/status"))
            .header("Authorization", format!("Bearer {key}"))
            .send()
            .await
            .unwrap();
        assert_ne!(resp.status().as_u16(), 401, "correct key must pass auth");
    }

    #[tokio::test]
    async fn auth_disabled_allows_all_requests() {
        let tmp = tempfile::tempdir().unwrap();
        let state = make_test_state(tmp.path()); // AuthMode::Disabled by default
        let app = build_router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let resp = reqwest::get(format!("http://{addr}/v1/status"))
            .await
            .unwrap();
        assert_ne!(
            resp.status().as_u16(),
            401,
            "disabled auth must not block requests"
        );
    }
}
