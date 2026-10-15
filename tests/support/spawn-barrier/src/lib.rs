//! Private test support for one fixed fork-to-exec barrier, outside AWMan's unsafe-free crate.

#![cfg(any(target_os = "macos", target_os = "linux"))]
#![deny(unsafe_op_in_unsafe_fn)]

use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread::{self, JoinHandle};

fn cloexec_pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut raw = [-1; 2];
    if unsafe { libc::pipe(raw.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let read = unsafe { OwnedFd::from_raw_fd(raw[0]) };
    let write = unsafe { OwnedFd::from_raw_fd(raw[1]) };
    for fd in [&read, &write] {
        if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok((read, write))
}

unsafe fn child_errno() -> i32 {
    #[cfg(target_os = "macos")]
    {
        unsafe { *libc::__error() }
    }
    #[cfg(target_os = "linux")]
    {
        unsafe { *libc::__errno_location() }
    }
}

fn wait_child(child: &mut Child) -> io::Result<ExitStatus> {
    loop {
        match child.wait() {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            result => return result,
        }
    }
}

struct ChildCustody(Option<Child>);

impl ChildCustody {
    fn wait(&mut self) -> io::Result<()> {
        let Some(child) = self.0.as_mut() else {
            return Err(io::Error::other("child was already reaped"));
        };
        let status = match wait_child(child) {
            Ok(status) => status,
            Err(error) => {
                let kill = child.kill();
                let reap = wait_child(child);
                if reap.is_ok() {
                    self.0.take();
                }
                return Err(io::Error::other(format!(
                    "child wait failed: {error}; kill: {kill:?}; reap: {reap:?}"
                )));
            }
        };
        self.0.take();
        if !status.success() {
            return Err(io::Error::other(format!(
                "barrier child exited with {status}"
            )));
        }
        Ok(())
    }
}

impl Drop for ChildCustody {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() {
            let _ = child.kill();
            let _ = wait_child(child);
        }
    }
}

/// A fixed `/usr/bin/true` child paused before exec, released and reaped on drop.
pub struct SpawnBarrier {
    release: Option<OwnedFd>,
    launch: Option<JoinHandle<io::Result<()>>>,
}

impl SpawnBarrier {
    /// Start the fixed benign command and wait until its child holds the pre-exec barrier.
    pub fn start() -> io::Result<Self> {
        let (ready_read, ready_write) = cloexec_pipe()?;
        let (release_read, release_write) = cloexec_pipe()?;
        let parent_ready = ready_read.as_raw_fd();
        let parent_release = release_write.as_raw_fd();
        let mut command = Command::new("/usr/bin/true");
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        unsafe {
            command.pre_exec(move || {
                // The fork child uses only async-signal-safe calls and its C errno slot.
                if libc::close(parent_ready) != 0 || libc::close(parent_release) != 0 {
                    libc::_exit(41);
                }
                loop {
                    let count = libc::write(ready_write.as_raw_fd(), b"R".as_ptr().cast(), 1);
                    if count == 1 {
                        break;
                    }
                    if count < 0 && child_errno() == libc::EINTR {
                        continue;
                    }
                    libc::_exit(42);
                }
                if libc::close(ready_write.as_raw_fd()) != 0 {
                    libc::_exit(43);
                }
                let mut byte = 0u8;
                loop {
                    let count =
                        libc::read(release_read.as_raw_fd(), (&mut byte as *mut u8).cast(), 1);
                    if count == 0 {
                        break;
                    }
                    if count < 0 && child_errno() == libc::EINTR {
                        continue;
                    }
                    libc::_exit(44);
                }
                if libc::close(release_read.as_raw_fd()) != 0 {
                    libc::_exit(45);
                }
                Ok(())
            });
        }
        let launch = thread::Builder::new()
            .name("sdk-unrelated-spawn".into())
            .spawn(move || {
                let result = match command.spawn() {
                    Ok(child) => ChildCustody(Some(child)).wait(),
                    Err(error) => Err(error),
                };
                // Spawn failure must close the last readiness writer before the parent waits again.
                drop(command);
                result
            })?;
        let mut barrier = Self {
            release: Some(release_write),
            launch: Some(launch),
        };
        let mut reader = File::from(ready_read);
        let mut ready = [0u8; 1];
        let readiness = reader.read_exact(&mut ready).and_then(|()| {
            if ready == *b"R" {
                Ok(())
            } else {
                Err(io::Error::other("invalid child barrier event"))
            }
        });
        if let Err(error) = readiness {
            let cleanup = barrier.release_and_wait();
            return Err(io::Error::other(format!(
                "child did not reach pre_exec barrier: {error}; cleanup: {cleanup:?}"
            )));
        }
        Ok(barrier)
    }

    /// Release the barrier and require actual exec, successful exit, and child reaping.
    pub fn release_and_wait(&mut self) -> io::Result<()> {
        // EOF releases the child without a write that could signal SIGPIPE.
        self.release.take();
        let Some(launch) = self.launch.take() else {
            return Ok(());
        };
        launch
            .join()
            .map_err(|_| io::Error::other("child launch thread panicked"))?
    }
}

impl Drop for SpawnBarrier {
    fn drop(&mut self) {
        if let Err(error) = self.release_and_wait() {
            if thread::panicking() {
                eprintln!("barrier cleanup failed during unwind: {error}");
            } else {
                panic!("barrier cleanup failed: {error}");
            }
        }
    }
}
