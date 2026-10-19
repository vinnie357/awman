"""Own isolated PTYs and bounded test subprocesses; never invoke a provider."""
import errno
import json
import os
import pty
import selectors
import subprocess
import sys
import time

executable, helper, mode = sys.argv[1:]
master = slave = None
child = None
stdout = stderr = terminal = b""
problem = None
code = 1
started = time.monotonic()
deadline = started + 8
total_deadline = started + 10
phases = []
reaped = False

def phase(name, **facts):
    record = dict(phase=name, wrapper_pid=os.getpid(),
                  child_pid=None if child is None else child.pid, **facts)
    phases.append(record)
    print(json.dumps(record), file=sys.stderr, flush=True)

def budget_left(limit):
    value = limit - time.monotonic()
    if value <= 0:
        raise TimeoutError("fixture deadline expired")
    return value

def available(stream, name):
    global stdout, stderr
    if stream.closed:
        return
    os.set_blocking(stream.fileno(), False)
    while True:
        try:
            chunk = os.read(stream.fileno(), 4096)
            if not chunk:
                return
            if name == "stdout": stdout += chunk
            else: stderr += chunk
            if time.monotonic() >= total_deadline:
                phase("pipe-capture-budget-ended", stream=name)
                return
        except BlockingIOError:
            return

def terminal_chunk():
    global terminal
    try:
        chunk = os.read(master, 4096)
    except BlockingIOError:
        return "idle"
    except OSError as error:
        if error.errno == errno.EIO:
            return "closed"
        raise
    terminal += chunk
    return "data" if chunk else "closed"

def drain_terminal():
    os.set_blocking(master, False)
    while True:
        budget_left(deadline)
        if terminal_chunk() != "data":
            return

def exchange(data=b""):
    global stdout, stderr
    pending = memoryview(data)
    with selectors.DefaultSelector() as events:
        for stream, name in ((child.stdout, "stdout"), (child.stderr, "stderr")):
            os.set_blocking(stream.fileno(), False)
            events.register(stream, selectors.EVENT_READ, name)
        if mode == "tty":
            os.set_blocking(master, False)
            events.register(master, selectors.EVENT_READ, "terminal")
        if pending:
            os.set_blocking(child.stdin.fileno(), False)
            events.register(child.stdin, selectors.EVENT_WRITE, "stdin")
        else:
            child.stdin.close()
        while any(key.data != "terminal" for key in events.get_map().values()):
            ready = events.select(budget_left(deadline))
            if not ready:
                raise subprocess.TimeoutExpired(child.args, 8)
            for key, _mask in ready:
                if key.data == "terminal":
                    if terminal_chunk() == "closed":
                        events.unregister(key.fd)
                    continue
                try:
                    if key.data == "stdin":
                        pending = pending[os.write(key.fd, pending):]
                        if pending:
                            continue
                        events.unregister(key.fileobj)
                        key.fileobj.close()
                    else:
                        chunk = os.read(key.fd, 4096)
                        if key.data == "stdout": stdout += chunk
                        else: stderr += chunk
                        if not chunk:
                            phase("child-pipe-eof", stream=key.data)
                            events.unregister(key.fileobj)
                            key.fileobj.close()
                except BlockingIOError:
                    continue
                except BrokenPipeError:
                    events.unregister(key.fileobj)
                    key.fileobj.close()
    if mode == "tty":
        drain_terminal()
        phase("terminal-drained-before-child-wait", captured_bytes=len(terminal))
    phase("child-pipes-drained-before-wait")
    child.wait(timeout=budget_left(deadline))

phase("wrapper-start", mode=mode)
try:
    env = {"AWMAN_AUTH_TERMINAL_TEST_CHILD": mode}
    if mode != "absent":
        master, slave = pty.openpty()
        phase("private-pty-created")
    if mode == "broken":
        env["AWMAN_AUTH_TERMINAL_TEST_SLAVE"] = os.ttyname(slave)
    # A new session owns only this fixture's slave. Ordinary output stays piped.
    bootstrap = """import os,sys,fcntl,termios
def checkpoint(name):
    os.write(2, ("BOOTSTRAP_PHASE " + name + "\\n").encode())
checkpoint("entered")
fd = int(sys.argv[1])
if fd >= 0:
    checkpoint("before-controlling-terminal-acquire")
    fcntl.ioctl(fd, termios.TIOCSCTTY, 0)
    checkpoint("after-controlling-terminal-acquire")
checkpoint("before-exec")
os.execve(sys.argv[2], sys.argv[2:], dict(os.environ))
"""
    controlling = slave if mode in ("tty", "idle") else -1
    child = subprocess.Popen(
        [sys.executable, "-c", bootstrap, str(controlling), executable,
         "--exact", helper, "--nocapture", "--quiet"],
        env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
        stderr=subprocess.PIPE, start_new_session=True,
        pass_fds=(() if controlling < 0 else (controlling,)),
    )
    phase("child-spawned")
    if mode in ("tty", "idle"):
        os.close(slave)
        slave = None
        phase("parent-slave-copy-closed")
    if mode == "broken":
        phase("waiting-private-sink-ready")
        ready = b"PRIVATE_AUTH_SINK_READY\n"
        with selectors.DefaultSelector() as events:
            events.register(child.stdout, selectors.EVENT_READ)
            while ready not in stdout:
                remaining = deadline - time.monotonic()
                if remaining <= 0 or not events.select(remaining):
                    raise TimeoutError("private sink readiness not observed")
                chunk = os.read(child.stdout.fileno(), 4096)
                if not chunk:
                    raise RuntimeError("private sink exited before validation handshake")
                stdout += chunk
        phase("private-sink-ready")
        os.close(slave)
        slave = None
        phase("parent-slave-copy-closed")
        os.close(master)
        master = None
        phase("private-master-closed")
        phase("waiting-child-exit")
        exchange(b"CLOSED\n")
    elif mode == "idle":
        phase("waiting-idle-writer-start")
        with selectors.DefaultSelector() as events:
            events.register(master, selectors.EVENT_READ)
            while b"PRIVATE_IDLE_WRITER_STARTED" not in terminal:
                remaining = deadline - time.monotonic()
                if remaining <= 0 or not events.select(remaining):
                    raise TimeoutError("idle writer start marker not observed")
                chunk = os.read(master, 4096)
                if not chunk:
                    raise RuntimeError("private terminal closed before writer start marker")
                terminal += chunk
        phase("idle-writer-start-observed")
        phase("waiting-child-exit")
        exchange(b"DROP\n")
    else:
        phase("waiting-child-exit")
        exchange()
    code = child.returncode
    reaped = True
    phase("child-exited", returncode=code)
    if mode == "tty":
        phase("terminal-capture-start")
        drain_terminal()
        phase("terminal-capture-complete", captured_bytes=len(terminal))
except (OSError, RuntimeError, TimeoutError, subprocess.TimeoutExpired) as error:
    child_phases = [line for line in stderr.decode(errors="replace").splitlines()
                    if line.startswith(("AUTH_TERMINAL_CHILD_PHASE ", "BOOTSTRAP_PHASE "))]
    phase("work-failed", exception=type(error).__name__, child_phases=child_phases)
    label = "IDLE_WRITER_SHUTDOWN_TIMEOUT" if mode == "idle" and isinstance(error, subprocess.TimeoutExpired) else "UNEXPECTED_TEST_ISSUE"
    problem = label + ": " + type(error).__name__ + ": " + str(error)
finally:
    phase("cleanup-start")
    if child is not None:
        cleanup_stage = "poll-owned-child"
        try:
            polled = child.poll()
            phase("owned-child-poll-returned", returncode=polled)
            if polled is None:
                cleanup_stage = "kill-owned-child"
                phase("owned-child-kill-requested")
                child.kill()
                phase("owned-child-kill-returned")
                cleanup_stage = "compute-wait-budget"
                wait_budget = budget_left(total_deadline)
                cleanup_stage = "wait-owned-child"
                phase("owned-child-wait-start", budget_seconds=wait_budget)
                child.wait(timeout=wait_budget)
                phase("owned-child-wait-returned", returncode=child.returncode)
            code = child.returncode
            reaped = child.returncode is not None
            phase("owned-child-reaped", returncode=code, reaped=reaped)
        except (OSError, TimeoutError, subprocess.TimeoutExpired) as error:
            detail = "OWNED_CHILD_CLEANUP_UNRESOLVED: " + type(error).__name__ + ": " + str(error)
            problem = detail if problem is None else problem + "; " + detail
            phase("owned-child-cleanup-unresolved", reaped=reaped,
                  stage=cleanup_stage, exception=type(error).__name__,
                  errno=getattr(error, "errno", None))
        for stream, name in ((child.stdout, "stdout"), (child.stderr, "stderr")):
            try:
                phase("owned-child-pipe-capture-start", stream=name)
                available(stream, name)
            except (OSError, TimeoutError, subprocess.TimeoutExpired) as error:
                detail = "PIPE_CAPTURE_UNRESOLVED: " + type(error).__name__ + ": " + str(error)
                problem = detail if problem is None else problem + "; " + detail
                phase("pipe-capture-unresolved", stream=name, exception=type(error).__name__)
        for stream in (child.stdin, child.stdout, child.stderr):
            if stream is not None:
                stream.close()
    for fd in (master, slave):
        if fd is not None:
            os.close(fd)
    phase("cleanup-complete", reaped=reaped)
if problem is not None and mode == "idle":
    problem += "; runtime_drop_ready=" + str(b"PRIVATE_IDLE_RUNTIME_DROP_READY\n" in stdout)
print(json.dumps(dict(returncode=code, stdout=list(stdout), stderr=list(stderr),
                      terminal=list(terminal), problem=problem, phases=phases,
                      child_pid=None if child is None else child.pid, reaped=reaped)))
