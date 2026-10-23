//! Issue 230: successful reader joins retain hard stderr errors.

use super::io_bridge::{bridge_piped_io, BridgeConfig, PipedChildIo, PipedReadEnd};
use crate::engine::agent_runtime::frontend::AgentIo;
use crate::engine::agent_runtime::output_tail::OutputTail;
use std::error::Error;
use std::io::{self, Read};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;
const IO_BOUND: Duration = Duration::from_secs(2);

struct LeadingError {
    _real_pipe: Box<dyn Read + Send>,
    calls: Arc<AtomicUsize>,
}

impl Read for LeadingError {
    fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(io::Error::other("injected leading stderr read error"))
    }
}

struct OwnedBridge {
    child: Child,
    readers: Vec<JoinHandle<PipedReadEnd>>,
}

impl Drop for OwnedBridge {
    fn drop(&mut self) {
        if !matches!(self.child.try_wait(), Ok(Some(_))) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        for reader in self.readers.drain(..) {
            let _ = reader.join();
        }
    }
}

#[tokio::test]
async fn leading_hard_stderr_error_is_a_read_error_even_when_thread_join_succeeds() -> TestResult {
    let child = Command::new("/bin/sh")
        .args(["-c", "exit 0"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut owned = OwnedBridge {
        child,
        readers: Vec::new(),
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let mut pipes = PipedChildIo::take(&mut owned.child);
    pipes.stderr = Some(Box::new(LeadingError {
        _real_pipe: pipes.stderr.take().ok_or("real stderr pipe missing")?,
        calls: Arc::clone(&calls),
    }));
    let (stdout, mut stdout_rx) = tokio::sync::mpsc::unbounded_channel();
    let (stderr, mut stderr_rx) = tokio::sync::mpsc::unbounded_channel();
    let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel();
    let bridge = bridge_piped_io(
        AgentIo {
            stdout,
            stderr,
            stdin_tx,
            stdin_rx,
            resize: None,
            initial_size: None,
        },
        pipes,
        BridgeConfig {
            grace_timeout: Duration::from_secs(10),
            stuck_timeout: Duration::from_secs(10),
            container_start_delay: Duration::ZERO,
            cancel_on_grace_expired: None,
            output_tail: Arc::new(OutputTail::with_default_capacity()),
            output_broadcast: None,
        },
    );
    owned.readers = bridge.output_readers;
    drop(bridge.stdin_injector);
    let status = tokio::time::timeout(IO_BOUND, async {
        loop {
            if let Some(status) = owned.child.try_wait()? {
                break Ok::<_, io::Error>(status);
            }
            tokio::task::yield_now().await;
        }
    })
    .await??;
    assert!(status.success());
    tokio::time::timeout(IO_BOUND, async {
        while owned.readers.iter().any(|reader| !reader.is_finished()) {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    assert_eq!(owned.readers.len(), 2);
    let mut eof = 0;
    let mut errors = 0;
    while let Some(reader) = owned.readers.pop() {
        match reader
            .join()
            .map_err(|_| "reader thread panicked instead of reporting its read end")?
        {
            PipedReadEnd::Eof => eof += 1,
            PipedReadEnd::ReadError => errors += 1,
        }
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(eof, 1);
    assert_eq!(errors, 1);
    assert!(tokio::time::timeout(IO_BOUND, stdout_rx.recv())
        .await?
        .is_none());
    assert!(tokio::time::timeout(IO_BOUND, stderr_rx.recv())
        .await?
        .is_none());
    Ok(())
}
