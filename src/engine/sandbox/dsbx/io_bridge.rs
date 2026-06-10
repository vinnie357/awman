//! Self-contained PTY / piped I/O bridge for the sandbox runtime tier.
//!
//! Deliberately a sibling of — not a reuse of — the container tier's
//! `io_bridge`. WI 0090's layering rule forbids `src/engine/sandbox/` from
//! importing `src/engine/container/`, so the sandbox tier carries its own
//! bridge. It is intentionally simpler than the container bridge: sandboxes
//! boot in 2–5s and the experimental runtime does not yet drive stuck/grace
//! detection, so this bridge wires the byte channels and hands back a
//! (currently undriven) stuck broadcast channel that satisfies
//! `AgentExecution`'s constructor.

use std::sync::{Arc, Mutex};

use crate::engine::agent_runtime::execution::StuckEvent;
use crate::engine::agent_runtime::frontend::AgentIo;
use crate::engine::error::EngineError;

/// Master PTY end, shared so the resize task and the execution backend can
/// both reach it.
pub(super) type PtyMaster = Arc<Mutex<Box<dyn portable_pty::MasterPty + Send>>>;

/// Artifacts the launch path stores for the execution lifetime.
pub(super) struct SandboxBridge {
    /// Sender feeding the stdin writer task; the engine keeps a clone for
    /// `try_inject_stdin`.
    pub stdin_injector: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    /// Broadcast sender for stuck events, stored in `AgentExecution`.
    pub stuck_tx: Arc<tokio::sync::broadcast::Sender<StuckEvent>>,
}

fn new_stuck_tx() -> Arc<tokio::sync::broadcast::Sender<StuckEvent>> {
    let (tx, _) = tokio::sync::broadcast::channel(8);
    Arc::new(tx)
}

/// Bridge a PTY master to the frontend's `AgentIo` channels.
///
/// Spawns a reader thread (PTY → `io.stdout`), a writer task
/// (`io.stdin_rx` → PTY), and a resize task (`io.resize` → `master.resize`).
pub(super) fn bridge_pty(
    io: AgentIo,
    pair: portable_pty::PtyPair,
) -> Result<(PtyMaster, SandboxBridge), EngineError> {
    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| EngineError::Sandbox(format!("clone sbx pty reader: {e}")))?;
    let mut writer = pair
        .master
        .take_writer()
        .map_err(|e| EngineError::Sandbox(format!("take sbx pty writer: {e}")))?;

    // Reader thread: PTY → stdout channel. Keep draining even if the sink dies
    // so the VM is never backpressured by a dead frontend.
    let stdout_tx = io.stdout;
    std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = [0u8; 4096];
        let mut sink_open = true;
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if sink_open && stdout_tx.send(buf[..n].to_vec()).is_err() {
                        sink_open = false;
                    }
                }
            }
        }
    });

    // Writer task: stdin channel → PTY.
    let stdin_tx = io.stdin_tx;
    let mut stdin_rx = io.stdin_rx;
    tokio::spawn(async move {
        use std::io::Write;
        while let Some(bytes) = stdin_rx.recv().await {
            if writer.write_all(&bytes).is_err() || writer.flush().is_err() {
                break;
            }
        }
    });

    // Resize task.
    let master_arc: PtyMaster = Arc::new(Mutex::new(pair.master));
    if let Some(mut resize_rx) = io.resize {
        let master_for_resize = Arc::clone(&master_arc);
        tokio::spawn(async move {
            use portable_pty::PtySize;
            while let Some((cols, rows)) = resize_rx.recv().await {
                if let Ok(master) = master_for_resize.lock() {
                    let _ = master.resize(PtySize {
                        rows,
                        cols,
                        pixel_width: 0,
                        pixel_height: 0,
                    });
                }
            }
        });
    }

    Ok((
        master_arc,
        SandboxBridge {
            stdin_injector: stdin_tx,
            stuck_tx: new_stuck_tx(),
        },
    ))
}

/// Bridge a piped child process to the frontend's `AgentIo` channels.
pub(super) fn bridge_piped(io: AgentIo, child: &mut std::process::Child) -> SandboxBridge {
    if let Some(child_stdout) = child.stdout.take() {
        let stdout_tx = io.stdout;
        std::thread::spawn(move || {
            use std::io::Read;
            let mut reader = child_stdout;
            let mut buf = [0u8; 4096];
            let mut sink_open = true;
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if sink_open && stdout_tx.send(buf[..n].to_vec()).is_err() {
                            sink_open = false;
                        }
                    }
                }
            }
        });
    }

    if let Some(child_stderr) = child.stderr.take() {
        let stderr_tx = io.stderr;
        std::thread::spawn(move || {
            use std::io::Read;
            let mut reader = child_stderr;
            let mut buf = [0u8; 4096];
            let mut sink_open = true;
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if sink_open && stderr_tx.send(buf[..n].to_vec()).is_err() {
                            sink_open = false;
                        }
                    }
                }
            }
        });
    }

    let stdin_tx = io.stdin_tx;
    if let Some(child_stdin) = child.stdin.take() {
        let mut stdin_rx = io.stdin_rx;
        tokio::spawn(async move {
            use std::io::Write;
            let mut writer = child_stdin;
            while let Some(bytes) = stdin_rx.recv().await {
                if writer.write_all(&bytes).is_err() || writer.flush().is_err() {
                    break;
                }
            }
        });
    }

    SandboxBridge {
        stdin_injector: stdin_tx,
        stuck_tx: new_stuck_tx(),
    }
}
