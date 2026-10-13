"""WI 0118 fixed-bootstrap tests. No container, network, or model is used."""

import hashlib
import importlib.util
import json
import os
from pathlib import Path
import select
import signal
import stat
import subprocess
import sys
import tempfile
import threading
import time
import types
import unittest
from unittest import mock


MODULE_PATH = (
    Path(__file__).resolve().parents[1]
    / "src"
    / "engine"
    / "container"
    / "startup_gate_bootstrap.py"
)
SPEC = importlib.util.spec_from_file_location("awman_startup_gate_bootstrap", MODULE_PATH)
bootstrap = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(bootstrap)


EMPTY_SHA = hashlib.sha256(b"").hexdigest()


def manifest_bytes(entries):
    return json.dumps(
        {"version": 1, "entries": entries}, separators=(",", ":")
    ).encode("utf-8")


def write_valid_control(control_path):
    manifest = manifest_bytes([])
    (control_path / "review.manifest.json").write_bytes(manifest)
    (control_path / "request.json").write_text(
        json.dumps(
            {
                "version": 1,
                "bindings": [
                    {
                        "id": "review-input",
                        "workspace_path": "/review/input",
                        "manifest_id": hashlib.sha256(manifest).hexdigest(),
                        "manifest_file": "review.manifest.json",
                        "access": "read-only",
                    }
                ],
            }
        ),
        encoding="utf-8",
    )
    (control_path / "review.manifest.json").chmod(0o600)
    (control_path / "request.json").chmod(0o600)


def write_v2_control(control_path, guest_cwd="/workspace"):
    write_valid_control(control_path)
    request_path = control_path / "request.json"
    request = json.loads(request_path.read_text(encoding="utf-8"))
    request["version"] = 2
    request["guest_cwd"] = guest_cwd
    write_protected_json(request_path, request)


def write_protected_json(path, value, padded_size=None):
    raw = json.dumps(value, separators=(",", ":")).encode("utf-8")
    if padded_size is not None:
        if len(raw) > padded_size:
            raise AssertionError("fixture payload exceeds requested padded size")
        raw += b" " * (padded_size - len(raw))
    path.write_bytes(raw)
    path.chmod(0o600)


def assert_protected_record(test, path, owner):
    info = path.stat(follow_symlinks=False)
    test.assertTrue(stat.S_ISREG(info.st_mode))
    test.assertEqual(stat.S_IMODE(info.st_mode), 0o600)
    test.assertEqual(info.st_nlink, 1)
    test.assertEqual((info.st_uid, info.st_gid), owner)


class FakeClock:
    def __init__(self):
        self.now = 0.0

    def monotonic(self):
        return self.now

    def sleep(self, seconds):
        self.now += seconds


class _OwnedObserverChildren:
    """Tracks only PIDs returned by real fork calls in the disposable runner."""

    def __init__(self, real_fork, real_kill, real_waitpid):
        self._real_fork = real_fork
        self._real_kill = real_kill
        self._real_waitpid = real_waitpid
        self._lock = threading.RLock()
        self.actual_pids = []
        self._owned_pids = set()
        self.production_killed = []
        self.production_reaped = []
        self.fallback_killed = []
        self.fallback_reaped = []
        self.fallback_not_owned = []
        self._before_fallback = None
        self._cleanup_active = False

    def fork(self):
        pid = self._real_fork()
        if pid > 0:
            with self._lock:
                self.actual_pids.append(pid)
                self._owned_pids.add(pid)
        return pid

    def kill(self, pid, signum):
        result = self._real_kill(pid, signum)
        with self._lock:
            self.production_killed.append(pid)
        return result

    def waitpid(self, pid, options):
        result = self._real_waitpid(pid, options)
        if result[0] == pid:
            with self._lock:
                self.production_reaped.append(pid)
                self._owned_pids.discard(pid)
        return result

    def cleanup(self):
        with self._lock:
            if self._cleanup_active:
                return False
            self._cleanup_active = True
            if self._before_fallback is None:
                self._before_fallback = {
                    "production_killed": list(self.production_killed),
                    "production_reaped": list(self.production_reaped),
                }
            pending = list(self._owned_pids)

        try:
            for pid in pending:
                with self._lock:
                    if pid not in self._owned_pids:
                        continue
                try:
                    reaped, _ = self._real_waitpid(pid, os.WNOHANG)
                except ChildProcessError:
                    with self._lock:
                        self._owned_pids.discard(pid)
                        self.fallback_not_owned.append(pid)
                    continue

                if reaped == pid:
                    with self._lock:
                        self._owned_pids.discard(pid)
                        self.fallback_reaped.append(pid)
                    continue
                if reaped != 0:
                    raise AssertionError(f"unexpected waitpid result for owned PID {pid}")

                self._real_kill(pid, signal.SIGKILL)
                with self._lock:
                    self.fallback_killed.append(pid)
                try:
                    reaped, _ = self._real_waitpid(pid, 0)
                except ChildProcessError:
                    with self._lock:
                        self._owned_pids.discard(pid)
                        self.fallback_not_owned.append(pid)
                    continue
                if reaped != pid:
                    raise AssertionError(f"failed to reap exact owned PID {pid}")
                with self._lock:
                    self._owned_pids.discard(pid)
                    self.fallback_reaped.append(pid)
        finally:
            with self._lock:
                self._cleanup_active = False
        return True

    def result_fields(self):
        with self._lock:
            before = self._before_fallback or {
                "production_killed": list(self.production_killed),
                "production_reaped": list(self.production_reaped),
            }
            return {
                "actual_pids": list(self.actual_pids),
                "production_killed_before_fallback": before["production_killed"],
                "production_reaped_before_fallback": before["production_reaped"],
                "fallback_killed": list(self.fallback_killed),
                "fallback_reaped": list(self.fallback_reaped),
                "fallback_not_owned": list(self.fallback_not_owned),
                "owned_pids_after_cleanup": sorted(self._owned_pids),
            }


def _observer_case_in_disposable_runner(case):
    real_fork = os.fork
    real_kill = os.kill
    real_waitpid = os.waitpid
    children = _OwnedObserverChildren(real_fork, real_kill, real_waitpid)
    latch = os.pipe() if case in {"timeout", "guard_idempotence"} else None
    child_started = (
        os.pipe()
        if case in {"timeout", "guard_idempotence", "guard_already_reaped"}
        else None
    )
    termination_requested = False
    result = {
        "status": "missing",
        "call_hung": False,
        "child_latch_entered": False,
    }

    def cleanup_owned_children(*_args):
        signal.setitimer(signal.ITIMER_REAL, 0)
        return children.cleanup()

    def terminate_runner(_signum, _frame):
        nonlocal termination_requested
        termination_requested = True
        if cleanup_owned_children():
            os._exit(124)

    def deadline_expired(_signum, _frame):
        result["call_hung"] = True
        cleanup_owned_children()
        if termination_requested:
            os._exit(124)
        raise TimeoutError("observer call exceeded disposable runner deadline")

    def hold_child(_identity):
        os.write(child_started[1], b"1")
        os.read(latch[0], 1)

    def fork_and_wait_for_child_latch():
        pid = children.fork()
        if pid > 0 and child_started is not None:
            readable, _, _ = select.select([child_started[0]], [], [], 1.0)
            if not readable or os.read(child_started[0], 1) != b"1":
                raise RuntimeError("observer child did not enter controlled latch")
            result["child_latch_entered"] = True
        return pid

    previous_alarm = signal.signal(signal.SIGALRM, deadline_expired)
    previous_term = signal.signal(signal.SIGTERM, terminate_runner)
    signal.setitimer(signal.ITIMER_REAL, 2.0)
    try:
        if case in {"guard_idempotence", "guard_already_reaped"}:
            pid = children.fork()
            if pid == 0:
                os.write(child_started[1], b"1")
                if latch is not None:
                    os.read(latch[0], 1)
                os._exit(0)
            readable, _, _ = select.select([child_started[0]], [], [], 1.0)
            if not readable or os.read(child_started[0], 1) != b"1":
                raise RuntimeError("guard child did not enter controlled latch")
            result["child_latch_entered"] = True
            if case == "guard_already_reaped":
                reaped, _ = real_waitpid(pid, 0)
                if reaped != pid:
                    raise RuntimeError("guard did not reap the exact child")
            cleanup_owned_children()
            result["first_cleanup"] = children.result_fields()
            cleanup_owned_children()
            result["second_cleanup"] = children.result_fields()
            result["status"] = "guard-ok"
        else:
            identity = bootstrap.resolve_runtime_identity(None)
            if case == "invalid_identity":
                identity = bootstrap.RuntimeIdentity(-1, -1, (), True)
            patches = [
                mock.patch.object(
                    bootstrap.os, "fork", side_effect=fork_and_wait_for_child_latch
                ),
                mock.patch.object(bootstrap.os, "kill", side_effect=children.kill),
                mock.patch.object(bootstrap.os, "waitpid", side_effect=children.waitpid),
            ]
            if case == "timeout":
                patches.append(
                    mock.patch.object(
                        bootstrap, "apply_runtime_identity", side_effect=hold_child
                    )
                )
            with patches[0], patches[1], patches[2]:
                if len(patches) == 4:
                    patches[3].start()
                try:
                    observation = bootstrap.observe_guest_cwd_as(
                        identity, **({"timeout": 0} if case == "timeout" else {})
                    )
                    result.update(
                        {
                            "status": "ok",
                            "cwd": observation.cwd,
                            "effective_uid": observation.effective_uid,
                            "effective_gid": observation.effective_gid,
                        }
                    )
                except bootstrap.GateError as error:
                    result.update({"status": "gate-error", "error_code": error.code})
                finally:
                    if len(patches) == 4:
                        patches[3].stop()
    except TimeoutError:
        result["status"] = "runner-deadline"
    finally:
        signal.setitimer(signal.ITIMER_REAL, 0)
        children.cleanup()
        if termination_requested:
            os._exit(124)
        if latch is not None:
            os.close(latch[0])
            os.close(latch[1])
        if child_started is not None:
            os.close(child_started[0])
            os.close(child_started[1])
        signal.signal(signal.SIGALRM, previous_alarm)
        signal.signal(signal.SIGTERM, previous_term)
    result.update(children.result_fields())
    return result


def _run_observer_case(case):
    read_fd, write_fd = os.pipe()
    runner_pid = os.fork()
    if runner_pid == 0:
        os.close(read_fd)
        try:
            payload = _observer_case_in_disposable_runner(case)
        except BaseException as error:
            payload = {"status": "runner-error", "detail": repr(error)}
        encoded = json.dumps(payload, separators=(",", ":")).encode("utf-8") + b"\n"
        try:
            os.write(write_fd, encoded)
        finally:
            os.close(write_fd)
        os._exit(0)

    os.close(write_fd)
    data = b""
    runner_reaped = False
    pipe_eof = False
    deadline = time.monotonic() + 4.0
    try:
        while not pipe_eof:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise AssertionError("disposable observer runner did not report")
            readable, _, _ = select.select([read_fd], [], [], remaining)
            if not readable:
                raise AssertionError("disposable observer runner did not report")
            chunk = os.read(read_fd, 16 * 1024 - len(data))
            if not chunk:
                pipe_eof = True
                break
            data += chunk
            if len(data) >= 16 * 1024:
                raise AssertionError("disposable observer runner result was oversized")
        waited, status = os.waitpid(runner_pid, 0)
        runner_reaped = waited == runner_pid
        if not runner_reaped or status != 0:
            raise AssertionError(f"disposable observer runner failed: {status}")
        if b"\n" not in data:
            raise AssertionError("disposable observer runner returned no complete result")
        return json.loads(data.split(b"\n", 1)[0])
    finally:
        if not runner_reaped:
            try:
                os.kill(runner_pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            cleanup_deadline = time.monotonic() + 1.0
            while not pipe_eof:
                remaining = cleanup_deadline - time.monotonic()
                if remaining <= 0:
                    break
                readable, _, _ = select.select([read_fd], [], [], remaining)
                if not readable:
                    break
                if not os.read(read_fd, 16 * 1024):
                    pipe_eof = True
            if pipe_eof:
                try:
                    waited, _ = os.waitpid(runner_pid, 0)
                    runner_reaped = waited == runner_pid
                except ChildProcessError:
                    runner_reaped = True
            if not runner_reaped:
                try:
                    os.kill(runner_pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                try:
                    os.waitpid(runner_pid, 0)
                except ChildProcessError:
                    pass
        os.close(read_fd)


class StartupGateBootstrapTest(unittest.TestCase):
    def test_load_request_preserves_v1_and_requires_exact_v2_guest_cwd(self):
        with tempfile.TemporaryDirectory() as control:
            control_path = Path(control)
            write_valid_control(control_path)
            self.assertNotIn("guest_cwd", bootstrap._load_request(control_path))

            write_v2_control(control_path)
            request = bootstrap._load_request(control_path)
            self.assertEqual(request["version"], 2)
            self.assertEqual(request["guest_cwd"], "/workspace")

            for value in [None, "", "workspace", "/review", "/workspace/"]:
                write_v2_control(control_path, value)
                with self.assertRaises(bootstrap.GateError) as raised:
                    bootstrap._load_request(control_path)
                self.assertEqual(raised.exception.code, "request-invalid")

            write_valid_control(control_path)
            raw = json.loads((control_path / "request.json").read_text(encoding="utf-8"))
            raw["version"] = 2
            write_protected_json(control_path / "request.json", raw)
            with self.assertRaises(bootstrap.GateError):
                bootstrap._load_request(control_path)

            write_v2_control(control_path)
            raw = json.loads((control_path / "request.json").read_text(encoding="utf-8"))
            raw["bindings"] = []
            write_protected_json(control_path / "request.json", raw)
            with self.assertRaises(bootstrap.GateError):
                bootstrap._load_request(control_path)

            for version in [True, 2.0, "2"]:
                write_v2_control(control_path)
                raw = json.loads(
                    (control_path / "request.json").read_text(encoding="utf-8")
                )
                raw["version"] = version
                write_protected_json(control_path / "request.json", raw)
                with self.assertRaises(bootstrap.GateError):
                    bootstrap._load_request(control_path)

            write_v2_control(control_path)
            raw = json.loads((control_path / "request.json").read_text(encoding="utf-8"))
            raw["extra"] = True
            write_protected_json(control_path / "request.json", raw)
            with self.assertRaises(bootstrap.GateError):
                bootstrap._load_request(control_path)

            write_v2_control(control_path)
            request_path = control_path / "request.json"
            raw = request_path.read_bytes()
            self.assertTrue(raw.endswith(b"}"))
            request_path.write_bytes(raw[:-1] + b',"guest_cwd":"/workspace"}')
            request_path.chmod(0o600)
            with self.assertRaises(bootstrap.GateError):
                bootstrap._load_request(control_path)

    def test_parse_manifest_binds_exact_raw_bytes_and_strict_v1_schema(self):
        raw = manifest_bytes(
            [
                {
                    "path": "README.md",
                    "kind": "file",
                    "size": 0,
                    "sha256": EMPTY_SHA,
                },
                {"path": "src", "kind": "directory", "size": 0, "sha256": None},
            ]
        )
        parsed = bootstrap.parse_manifest(raw, hashlib.sha256(raw).hexdigest())
        self.assertEqual([e["path"] for e in parsed["entries"]], ["README.md", "src"])
        with self.assertRaises(Exception):
            bootstrap.parse_manifest(raw + b"\n", hashlib.sha256(raw).hexdigest())
        with self.assertRaises(Exception):
            bootstrap.parse_manifest(b"\xef\xbb\xbf" + raw, hashlib.sha256(b"\xef\xbb\xbf" + raw).hexdigest())
        with self.assertRaises(Exception):
            bootstrap.parse_manifest(b'{"version":1,"entries":[],"extra":true}', hashlib.sha256(b'{"version":1,"entries":[],"extra":true}').hexdigest())

    def test_mountinfo_uses_most_specific_mount_and_enforces_effective_access(self):
        mountinfo = "\n".join(
            [
                "10 1 0:1 / /review rw,relatime - bind /host rw",
                "11 10 0:2 / /review/input ro,relatime - bind /prepared ro",
            ]
        )
        read_only = {"workspace_path": "/review/input", "access": "read-only"}
        bootstrap.validate_mount(read_only, mountinfo)
        with self.assertRaises(Exception):
            bootstrap.validate_mount(
                {"workspace_path": "/review/input", "access": "read-write"},
                mountinfo,
            )

        writable = "11 1 0:2 / /work rw,relatime - bind /prepared rw"
        bootstrap.validate_mount(
            {"workspace_path": "/work", "access": "read-write"}, writable
        )
        with self.assertRaises(Exception):
            bootstrap.validate_mount(
                {"workspace_path": "/work", "access": "read-only"}, writable
            )

    def test_mountinfo_rejects_unrequested_nested_writable_exposure(self):
        mountinfo = "\n".join(
            [
                "11 1 0:2 / /review/input ro,relatime - bind /prepared ro",
                "12 11 0:3 / /review/input/secret rw,relatime - bind /other rw",
            ]
        )
        with self.assertRaises(Exception):
            bootstrap.validate_mount(
                {"workspace_path": "/review/input", "access": "read-only"},
                mountinfo,
            )

    def test_verify_tree_requires_the_exact_full_guest_tree(self):
        with tempfile.TemporaryDirectory() as root:
            root_path = Path(root)
            (root_path / "src").mkdir()
            (root_path / "README.md").write_bytes(b"abc")
            manifest = {
                "version": 1,
                "entries": [
                    {
                        "path": "README.md",
                        "kind": "file",
                        "size": 3,
                        "sha256": hashlib.sha256(b"abc").hexdigest(),
                    },
                    {"path": "src", "kind": "directory", "size": 0, "sha256": None},
                ],
            }
            bootstrap.verify_tree(root_path, manifest)
            (root_path / "extra.txt").write_text("extra", encoding="utf-8")
            with self.assertRaises(Exception):
                bootstrap.verify_tree(root_path, manifest)
            (root_path / "extra.txt").unlink()
            (root_path / "README.md").write_bytes(b"abd")
            with self.assertRaises(Exception):
                bootstrap.verify_tree(root_path, manifest)

    def test_verify_tree_rejects_symlinks_and_multiply_linked_files(self):
        with tempfile.TemporaryDirectory() as root:
            root_path = Path(root)
            target = root_path / "target"
            target.write_bytes(b"")
            (root_path / "link").symlink_to(target)
            symlink_manifest = {
                "version": 1,
                "entries": [
                    {"path": "link", "kind": "file", "size": 0, "sha256": EMPTY_SHA},
                    {"path": "target", "kind": "file", "size": 0, "sha256": EMPTY_SHA},
                ],
            }
            with self.assertRaises(Exception):
                bootstrap.verify_tree(root_path, symlink_manifest)
            (root_path / "link").unlink()
            os.link(target, root_path / "alias")
            hardlink_manifest = {
                "version": 1,
                "entries": [
                    {"path": "alias", "kind": "file", "size": 0, "sha256": EMPTY_SHA},
                    {"path": "target", "kind": "file", "size": 0, "sha256": EMPTY_SHA},
                ],
            }
            with self.assertRaises(Exception):
                bootstrap.verify_tree(root_path, hardlink_manifest)

    def test_await_release_requires_matching_nonce_and_removes_release(self):
        with tempfile.TemporaryDirectory() as control:
            control_path = Path(control)
            ready = {"version": 1, "nonce": "a" * 64}
            write_protected_json(
                control_path / "release.json",
                {"version": 1, "nonce": ready["nonce"]},
            )
            clock = FakeClock()
            bootstrap.await_release(
                control_path,
                ready,
                10,
                clock=clock.monotonic,
                sleep=clock.sleep,
            )
            self.assertFalse((control_path / "release.json").exists())

            write_protected_json(
                control_path / "release.json",
                {"version": 1, "nonce": "b" * 64},
            )
            with self.assertRaises(Exception):
                bootstrap.await_release(
                    control_path,
                    ready,
                    10,
                    clock=clock.monotonic,
                    sleep=clock.sleep,
                )

            invalid_releases = [
                {"version": 2, "nonce": ready["nonce"]},
                {"version": 1, "nonce": ready["nonce"], "extra": True},
                {"version": 1},
            ]
            for release in invalid_releases:
                (control_path / "release.json").write_text(
                    json.dumps(release), encoding="utf-8"
                )
                with self.assertRaises(Exception):
                    bootstrap.await_release(
                        control_path,
                        ready,
                        10,
                        clock=clock.monotonic,
                        sleep=clock.sleep,
                    )

    def test_await_release_timeout_uses_injected_monotonic_clock(self):
        with tempfile.TemporaryDirectory() as control:
            clock = FakeClock()
            with self.assertRaises(Exception) as raised:
                bootstrap.await_release(
                    Path(control),
                    {"version": 1, "nonce": "a" * 64},
                    2,
                    clock=clock.monotonic,
                    sleep=clock.sleep,
                )
            self.assertIn("timeout", str(raised.exception).lower())
            self.assertGreaterEqual(clock.now, 2)

    def test_run_gate_cancellation_reaches_wait_and_never_execs(self):
        calls = []
        events = []
        cancel_observations = []

        def forbidden_exec(*args):
            calls.append(args)

        with tempfile.TemporaryDirectory() as control:
            control_path = Path(control)
            write_valid_control(control_path)
            clock = FakeClock()

            def cancel_after_ready():
                ready_exists = (control_path / "ready.json").exists()
                cancel_observations.append(ready_exists)
                return ready_exists

            def verified(*_args):
                events.append("verify")

            def probed(*_args):
                events.append("probe")

            with mock.patch.object(bootstrap, "validate_mount", verified), mock.patch.object(
                bootstrap, "verify_tree", verified
            ), mock.patch.object(bootstrap, "probe_access", probed):
                with self.assertRaises(bootstrap.GateError) as raised:
                    bootstrap.run_gate(
                        control_path,
                        ["agent", "space value", "line\nbreak", "--leading"],
                        {"PYTHONHOME": "/original/home", "PYTHONPATH": "/original/path"},
                        "fixture mountinfo",
                        container_name="fixture-container",
                        cancel_check=cancel_after_ready,
                        clock=clock.monotonic,
                        sleep=clock.sleep,
                        exec_fn=forbidden_exec,
                    )
            self.assertTrue((control_path / "ready.json").exists())
            self.assertEqual(raised.exception.code, "cancelled")
            failure = json.loads((control_path / "failure.json").read_text(encoding="utf-8"))
            self.assertEqual(failure["code"], "cancelled")
        self.assertEqual(events, ["verify", "verify", "probe"])
        self.assertTrue(cancel_observations)
        self.assertTrue(cancel_observations[-1])
        self.assertEqual(calls, [], "cancellation must not execute the agent")

    def test_run_gate_orders_verify_ready_release_then_exact_exec(self):
        events = []
        exec_calls = []
        original_argv = ["agent", "space value", "quote'\"", "line\nbreak", "--leading", ""]
        original_env = {
            "SAFE": "value with spaces",
            "PYTHONHOME": "/original/home",
            "PYTHONPATH": "/original/path",
        }

        def validate(*_args):
            events.append("mount")

        def verify(*_args):
            events.append("tree")

        def probe(*_args):
            events.append("probe")

        def release(control_dir, ready, _timeout, **_kwargs):
            events.append("ready")
            self.assertTrue((control_dir / "ready.json").exists())
            self.assertEqual(
                set(ready), {"version", "nonce", "container_name", "bindings"}
            )
            self.assertEqual(ready["version"], 1)
            self.assertRegex(ready["nonce"], r"^[0-9a-f]{64}$")
            self.assertNotIn("host", json.dumps(ready).lower())
            self.assertEqual(exec_calls, [], "agent must not exec before release")
            events.append("release")

        def capture_exec(file, argv, env):
            events.append("exec")
            exec_calls.append((file, list(argv), dict(env)))

        with tempfile.TemporaryDirectory() as control:
            control_path = Path(control)
            write_valid_control(control_path)
            with mock.patch.object(bootstrap, "validate_mount", validate), mock.patch.object(
                bootstrap, "verify_tree", verify
            ), mock.patch.object(bootstrap, "probe_access", probe), mock.patch.object(
                bootstrap, "await_release", release
            ):
                bootstrap.run_gate(
                    control_path,
                    original_argv,
                    original_env,
                    "fixture mountinfo",
                    container_name="fixture-container",
                    exec_fn=capture_exec,
                )
            ready_bytes = (control_path / "ready.json").read_bytes()
            ready = json.loads(ready_bytes)
            self.assertEqual(
                ready_bytes,
                json.dumps(ready, separators=(",", ":"), sort_keys=True).encode("utf-8"),
            )
            self.assertNotIn("guest_cwd", ready)

        self.assertEqual(events, ["mount", "tree", "probe", "ready", "release", "exec"])
        self.assertEqual(exec_calls, [(original_argv[0], original_argv, original_env)])

    def test_guest_cwd_observer_uses_a_real_bounded_runtime_identity_child(self):
        result = _run_observer_case("success")

        self.assertEqual(result["status"], "ok")
        self.assertFalse(result["call_hung"])
        self.assertEqual(Path(result["cwd"]), Path(os.getcwd()))
        self.assertEqual(result["effective_uid"], os.geteuid())
        self.assertEqual(result["effective_gid"], os.getegid())
        self.assertEqual(len(result["actual_pids"]), 1)
        self.assertEqual(
            result["production_reaped_before_fallback"], result["actual_pids"]
        )
        self.assertEqual(result["fallback_killed"], [])
        self.assertEqual(result["fallback_reaped"], [])

    def test_guest_cwd_observer_timeout_and_child_error_reap_the_owned_child(self):
        timeout = _run_observer_case("timeout")
        self.assertEqual(timeout["status"], "gate-error")
        self.assertEqual(timeout["error_code"], "identity-probe")
        self.assertFalse(timeout["call_hung"])
        self.assertTrue(timeout["child_latch_entered"])
        self.assertEqual(len(timeout["actual_pids"]), 1)
        self.assertEqual(
            timeout["production_killed_before_fallback"], timeout["actual_pids"]
        )
        self.assertEqual(
            timeout["production_reaped_before_fallback"], timeout["actual_pids"]
        )
        self.assertEqual(timeout["fallback_killed"], [])
        self.assertEqual(timeout["fallback_reaped"], [])

        invalid = _run_observer_case("invalid_identity")
        self.assertEqual(invalid["status"], "gate-error")
        self.assertEqual(invalid["error_code"], "identity-probe")
        self.assertFalse(invalid["call_hung"])
        self.assertEqual(len(invalid["actual_pids"]), 1)
        self.assertEqual(
            invalid["production_reaped_before_fallback"], invalid["actual_pids"]
        )
        self.assertEqual(invalid["fallback_killed"], [])
        self.assertEqual(invalid["fallback_reaped"], [])

    def test_observer_child_guard_consumes_pid_ownership_once(self):
        result = _run_observer_case("guard_idempotence")
        self.assertEqual(result["status"], "guard-ok")
        self.assertTrue(result["child_latch_entered"])
        self.assertEqual(len(result["actual_pids"]), 1)
        self.assertEqual(result["production_killed_before_fallback"], [])
        self.assertEqual(result["production_reaped_before_fallback"], [])
        self.assertEqual(result["fallback_killed"], result["actual_pids"])
        self.assertEqual(result["fallback_reaped"], result["actual_pids"])
        self.assertEqual(result["fallback_not_owned"], [])
        self.assertEqual(result["owned_pids_after_cleanup"], [])
        self.assertEqual(result["second_cleanup"], result["first_cleanup"])

        already_reaped = _run_observer_case("guard_already_reaped")
        self.assertEqual(already_reaped["status"], "guard-ok")
        self.assertEqual(len(already_reaped["actual_pids"]), 1)
        self.assertEqual(already_reaped["production_reaped_before_fallback"], [])
        self.assertEqual(already_reaped["fallback_killed"], [])
        self.assertEqual(already_reaped["fallback_reaped"], [])
        self.assertEqual(
            already_reaped["fallback_not_owned"], already_reaped["actual_pids"]
        )
        self.assertEqual(already_reaped["owned_pids_after_cleanup"], [])
        self.assertEqual(
            already_reaped["second_cleanup"], already_reaped["first_cleanup"]
        )

    def test_v2_orders_runtime_identity_cwd_observation_before_ready_and_immediately_before_exec(self):
        events = []
        exec_calls = []
        runtime_identity = object()

        def observe_cwd(actual_identity):
            self.assertIs(actual_identity, runtime_identity)
            events.append("cwd")
            return bootstrap.GuestCwdObservation("/workspace", 1000, 1000)

        def release(control_dir, ready, _timeout, **_kwargs):
            events.append("release")
            self.assertEqual(ready["version"], 2)
            self.assertEqual(ready["guest_cwd"], "/workspace")
            self.assertEqual(exec_calls, [])

        def capture_exec(file, argv, env):
            events.append("exec")
            exec_calls.append((file, list(argv), dict(env)))

        with tempfile.TemporaryDirectory() as control:
            control_path = Path(control)
            write_v2_control(control_path)
            with mock.patch.object(
                bootstrap, "validate_mount", return_value=None
            ), mock.patch.object(
                bootstrap, "verify_binding_as", return_value=None
            ), mock.patch.object(
                bootstrap, "observe_guest_cwd_as", side_effect=observe_cwd
            ), mock.patch.object(
                bootstrap, "await_release", side_effect=release
            ):
                bootstrap.run_gate(
                    control_path,
                    ["agent", "--exact"],
                    {"SAFE": "kept"},
                    "fixture mountinfo",
                    runtime_identity=runtime_identity,
                    container_name="fixture-container",
                    exec_fn=capture_exec,
                )

            ready = json.loads((control_path / "ready.json").read_text(encoding="utf-8"))

        self.assertEqual(ready["guest_cwd"], "/workspace")
        self.assertEqual(events, ["cwd", "release", "cwd", "exec"])
        self.assertEqual(exec_calls, [("agent", ["agent", "--exact"], {"SAFE": "kept"})])

    def test_v2_cwd_mismatch_before_ready_or_after_release_never_executes_provider(self):
        runtime_identity = object()
        cases = [
            (["/wrong"], False),
            (["/workspace", "/changed"], True),
        ]
        for observations, ready_expected in cases:
            with self.subTest(observations=observations):
                exec_calls = []
                observed = iter(observations)
                with tempfile.TemporaryDirectory() as control:
                    control_path = Path(control)
                    write_v2_control(control_path)
                    with mock.patch.object(
                        bootstrap, "validate_mount", return_value=None
                    ), mock.patch.object(
                        bootstrap, "verify_binding_as", return_value=None
                    ), mock.patch.object(
                        bootstrap,
                        "observe_guest_cwd_as",
                        side_effect=lambda *_args: bootstrap.GuestCwdObservation(
                            next(observed), 1000, 1000
                        ),
                    ), mock.patch.object(
                        bootstrap, "await_release", return_value=None
                    ):
                        with self.assertRaises(bootstrap.GateError) as raised:
                            bootstrap.run_gate(
                                control_path,
                                ["agent"],
                                {},
                                "fixture mountinfo",
                                runtime_identity=runtime_identity,
                                container_name="fixture-container",
                                exec_fn=lambda *args: exec_calls.append(args),
                            )

                    self.assertEqual(raised.exception.code, "cwd-invalid")
                    self.assertEqual((control_path / "ready.json").exists(), ready_expected)
                    failure = json.loads(
                        (control_path / "failure.json").read_text(encoding="utf-8")
                    )
                    self.assertEqual(failure["code"], "cwd-invalid")
                    self.assertEqual(exec_calls, [])


class StartupGateGuestControlTest(unittest.TestCase):
    VALID_MOUNTINFO = "11 1 0:2 / /review/input ro,relatime - bind /prepared ro"
    FIFO_CHILD = "AWMAN_STARTUP_GATE_FIFO_CHILD"

    def test_guest_owner_comes_from_held_directory_and_controls_written_records(self):
        exec_calls = []
        with tempfile.TemporaryDirectory() as parent:
            parent_path = Path(parent)
            approved = parent_path / "approved"
            control = parent_path / "control"
            approved.mkdir(mode=0o700)
            control.mkdir(mode=0o700)
            write_valid_control(approved)
            directory = control.stat(follow_symlinks=False)
            derived_owner = (directory.st_uid, directory.st_gid)
            untrusted_owner = (directory.st_uid + 1000, directory.st_gid + 1000)
            clock = FakeClock()

            def release_after_ready():
                ready_path = control / "ready.json"
                release_path = control / "release.json"
                if ready_path.exists() and not release_path.exists():
                    ready = json.loads(ready_path.read_text(encoding="utf-8"))
                    write_protected_json(
                        release_path,
                        {"version": 1, "nonce": ready["nonce"]},
                    )
                return False

            with mock.patch.object(
                bootstrap, "validate_mount", return_value=None
            ), mock.patch.object(
                bootstrap, "verify_tree", return_value=None
            ), mock.patch.object(
                bootstrap, "probe_access", return_value=None
            ):
                bootstrap.run_gate(
                    control,
                    ["agent", "space value"],
                    {"SAFE": "kept"},
                    self.VALID_MOUNTINFO,
                    approved_dir=approved,
                    container_name="fixture-container",
                    control_owner=untrusted_owner,
                    cancel_check=release_after_ready,
                    clock=clock.monotonic,
                    sleep=clock.sleep,
                    exec_fn=lambda file, argv, env: exec_calls.append(
                        (file, list(argv), dict(env))
                    ),
                )

            self.assertEqual(
                exec_calls,
                [("agent", ["agent", "space value"], {"SAFE": "kept"})],
            )
            self.assertFalse((control / "release.json").exists())
            self.assertFalse((control / "failure.json").exists())
            assert_protected_record(self, control / "ready.json", derived_owner)
            assert_protected_record(self, control / ".released", derived_owner)
            self.assertNotEqual(derived_owner, untrusted_owner)

    def test_control_handle_revalidates_identity_and_directory_mode(self):
        for field in ("uid", "gid"):
            with self.subTest(field=field), tempfile.TemporaryDirectory() as control_name:
                control = Path(control_name)
                control.chmod(0o700)
                write_protected_json(
                    control / "release.json",
                    {"version": 1, "nonce": "a" * 64},
                )
                fd = os.open(
                    control,
                    os.O_RDONLY
                    | getattr(os, "O_DIRECTORY", 0)
                    | getattr(os, "O_NOFOLLOW", 0),
                )
                info = os.fstat(fd)
                expected_uid = info.st_uid + (1 if field == "uid" else 0)
                expected_gid = info.st_gid + (1 if field == "gid" else 0)
                wrong_owner = bootstrap.ControlDirectory(
                    fd, expected_uid, expected_gid
                )
                try:
                    with self.assertRaises(bootstrap.GateError):
                        bootstrap._read_control_record(
                            wrong_owner,
                            "release.json",
                            4096,
                            "release-invalid",
                        )
                finally:
                    wrong_owner.close()

        with tempfile.TemporaryDirectory() as control_name:
            control = Path(control_name)
            control.chmod(0o755)
            with self.assertRaises(bootstrap.GateError):
                bootstrap.open_control_directory(control)

        with tempfile.TemporaryDirectory() as parent:
            parent_path = Path(parent)
            actual = parent_path / "actual"
            actual.mkdir(mode=0o700)
            link = parent_path / "control-link"
            link.symlink_to(actual, target_is_directory=True)
            with self.assertRaises(bootstrap.GateError):
                bootstrap.open_control_directory(link)

    def test_control_record_rejects_owner_different_from_held_directory(self):
        for field, index in (("uid", 4), ("gid", 5)):
            with self.subTest(field=field), tempfile.TemporaryDirectory() as control_name:
                control = Path(control_name)
                control.chmod(0o700)
                write_protected_json(
                    control / "release.json",
                    {"version": 1, "nonce": "a" * 64},
                )
                control_handle = bootstrap.open_control_directory(control)
                real_open = bootstrap.os.open
                real_fstat = bootstrap.os.fstat
                record_fds = set()

                def track_record_open(path, flags, mode=0o777, *, dir_fd=None):
                    fd = real_open(path, flags, mode, dir_fd=dir_fd)
                    if (
                        os.fspath(path) == "release.json"
                        and dir_fd == control_handle.fd
                    ):
                        record_fds.add(fd)
                    return fd

                def wrong_record_owner(fd):
                    info = real_fstat(fd)
                    if fd not in record_fds:
                        return info
                    fields = list(info)
                    fields[index] += 1
                    return os.stat_result(fields)

                try:
                    with mock.patch.object(
                        bootstrap.os, "open", track_record_open
                    ), mock.patch.object(bootstrap.os, "fstat", wrong_record_owner):
                        with self.assertRaises(bootstrap.GateError) as raised:
                            bootstrap._read_control_record(
                                control_handle,
                                "release.json",
                                4096,
                                "release-invalid",
                            )
                    self.assertEqual(raised.exception.code, "release-invalid")
                finally:
                    control_handle.close()

    def test_control_record_rejects_wrong_mode_links_and_valid_oversize_json(self):
        cases = ("mode", "symlink", "hardlink", "oversize")
        for case in cases:
            with self.subTest(case=case), tempfile.TemporaryDirectory() as control_name:
                control = Path(control_name)
                control.chmod(0o700)
                release = control / "release.json"
                value = {"version": 1, "nonce": "a" * 64}
                if case == "symlink":
                    target = control / "target.json"
                    write_protected_json(target, value)
                    release.symlink_to(target.name)
                elif case == "hardlink":
                    target = control / "target.json"
                    write_protected_json(target, value)
                    os.link(target, release)
                elif case == "oversize":
                    write_protected_json(release, value, padded_size=4097)
                else:
                    write_protected_json(release, value)
                    release.chmod(0o640)

                control_handle = bootstrap.open_control_directory(control)
                try:
                    with self.assertRaises(bootstrap.GateError) as raised:
                        bootstrap._read_control_record(
                            control_handle,
                            "release.json",
                            4096,
                            "release-invalid",
                        )
                    self.assertEqual(raised.exception.code, "release-invalid")
                finally:
                    control_handle.close()

    def test_control_record_fifo_is_rejected_without_blocking(self):
        if os.environ.get(self.FIFO_CHILD) != "1":
            environment = dict(os.environ)
            environment[self.FIFO_CHILD] = "1"
            completed = subprocess.run(
                [
                    sys.executable,
                    str(Path(__file__).resolve()),
                    f"{type(self).__name__}.{self._testMethodName}",
                ],
                env=environment,
                capture_output=True,
                text=True,
                timeout=5,
                check=False,
            )
            self.assertEqual(
                completed.returncode,
                0,
                f"FIFO child failed: {completed.stdout}\n{completed.stderr}",
            )
            return

        with tempfile.TemporaryDirectory() as control_name:
            control = Path(control_name)
            control.chmod(0o700)
            os.mkfifo(control / "release.json", mode=0o600)
            control_handle = bootstrap.open_control_directory(control)
            try:
                with self.assertRaises(bootstrap.GateError) as raised:
                    bootstrap._read_control_record(
                        control_handle,
                        "release.json",
                        4096,
                        "release-invalid",
                    )
                self.assertEqual(raised.exception.code, "release-invalid")
            finally:
                control_handle.close()

    def test_control_record_rejects_inode_replacement_between_stat_and_open(self):
        with tempfile.TemporaryDirectory() as control_name:
            control = Path(control_name)
            control.chmod(0o700)
            write_protected_json(
                control / "release.json",
                {"version": 1, "nonce": "a" * 64},
            )
            write_protected_json(
                control / ".replacement",
                {"version": 1, "nonce": "b" * 64},
            )
            control_handle = bootstrap.open_control_directory(control)
            real_open = bootstrap.os.open
            swapped = []

            def replace_before_open(path, flags, mode=0o777, *, dir_fd=None):
                if (
                    os.fspath(path) == "release.json"
                    and dir_fd == control_handle.fd
                    and not swapped
                ):
                    os.replace(
                        ".replacement",
                        "release.json",
                        src_dir_fd=control_handle.fd,
                        dst_dir_fd=control_handle.fd,
                    )
                    swapped.append(True)
                return real_open(path, flags, mode, dir_fd=dir_fd)

            try:
                with mock.patch.object(bootstrap.os, "open", replace_before_open):
                    with self.assertRaises(bootstrap.GateError) as raised:
                        bootstrap._read_control_record(
                            control_handle,
                            "release.json",
                            4096,
                            "release-invalid",
                        )
                self.assertEqual(raised.exception.code, "release-invalid")
                self.assertEqual(swapped, [True])
            finally:
                control_handle.close()

    def test_held_control_directory_survives_path_replacement_for_all_records(self):
        exec_calls = []
        with tempfile.TemporaryDirectory() as parent:
            parent_path = Path(parent)
            approved = parent_path / "approved"
            original_control = parent_path / "control"
            held_control = parent_path / "held-control"
            approved.mkdir(mode=0o700)
            original_control.mkdir(mode=0o700)
            write_valid_control(approved)
            renamed = []
            clock = FakeClock()

            def replace_control_path(*_args):
                if renamed:
                    return
                original_control.rename(held_control)
                original_control.mkdir(mode=0o700)
                renamed.append(True)

            def release_after_ready():
                ready_path = held_control / "ready.json"
                held_release = held_control / "release.json"
                attacker_release = original_control / "release.json"
                if ready_path.exists() and not held_release.exists():
                    ready = json.loads(ready_path.read_text(encoding="utf-8"))
                    write_protected_json(
                        held_release,
                        {"version": 1, "nonce": ready["nonce"]},
                    )
                    write_protected_json(
                        attacker_release,
                        {"version": 1, "nonce": "0" * 64},
                    )
                return False

            with mock.patch.object(
                bootstrap, "validate_mount", side_effect=replace_control_path
            ), mock.patch.object(
                bootstrap, "verify_tree", return_value=None
            ), mock.patch.object(
                bootstrap, "probe_access", return_value=None
            ):
                bootstrap.run_gate(
                    original_control,
                    ["agent"],
                    {},
                    self.VALID_MOUNTINFO,
                    approved_dir=approved,
                    container_name="fixture-container",
                    cancel_check=release_after_ready,
                    clock=clock.monotonic,
                    sleep=clock.sleep,
                    exec_fn=lambda *args: exec_calls.append(args),
                )

            self.assertEqual(renamed, [True])
            self.assertEqual(len(exec_calls), 1)
            self.assertTrue((held_control / "ready.json").exists())
            self.assertTrue((held_control / ".released").exists())
            self.assertFalse((held_control / "release.json").exists())
            self.assertFalse((held_control / "failure.json").exists())
            self.assertTrue((original_control / "release.json").exists())
            self.assertFalse((original_control / "ready.json").exists())
            self.assertFalse((original_control / ".released").exists())
            self.assertFalse((original_control / "failure.json").exists())

    def test_held_control_directory_receives_timeout_and_cancel_failures_after_rename(self):
        for expected_code in ("cancelled", "timeout"):
            with self.subTest(expected_code=expected_code), tempfile.TemporaryDirectory() as parent:
                parent_path = Path(parent)
                approved = parent_path / "approved"
                original_control = parent_path / "control"
                held_control = parent_path / "held-control"
                approved.mkdir(mode=0o700)
                original_control.mkdir(mode=0o700)
                write_valid_control(approved)
                renamed = []
                exec_calls = []
                clock = FakeClock()

                def replace_control_path(*_args):
                    original_control.rename(held_control)
                    original_control.mkdir(mode=0o700)
                    renamed.append(True)

                def cancel_after_rename():
                    return expected_code == "cancelled" and bool(renamed)

                with mock.patch.object(
                    bootstrap, "validate_mount", side_effect=replace_control_path
                ), mock.patch.object(
                    bootstrap, "verify_tree", return_value=None
                ), mock.patch.object(
                    bootstrap, "probe_access", return_value=None
                ):
                    with self.assertRaises(bootstrap.GateError) as raised:
                        bootstrap.run_gate(
                            original_control,
                            ["agent"],
                            {},
                            self.VALID_MOUNTINFO,
                            approved_dir=approved,
                            container_name="fixture-container",
                            cancel_check=cancel_after_rename,
                            clock=clock.monotonic,
                            sleep=clock.sleep,
                            timeout=1,
                            exec_fn=lambda *args: exec_calls.append(args),
                        )

                self.assertEqual(raised.exception.code, expected_code)
                self.assertEqual(renamed, [True])
                self.assertEqual(exec_calls, [])
                self.assertTrue((held_control / "ready.json").exists())
                self.assertFalse((held_control / ".released").exists())
                self.assertFalse((held_control / "release.json").exists())
                failure = json.loads(
                    (held_control / "failure.json").read_text(encoding="utf-8")
                )
                self.assertEqual(failure["code"], expected_code)
                held_owner = held_control.stat(follow_symlinks=False)
                assert_protected_record(
                    self,
                    held_control / "failure.json",
                    (held_owner.st_uid, held_owner.st_gid),
                )
                self.assertEqual(list(original_control.iterdir()), [])

    def test_main_rejects_host_owner_option_and_drops_privileges_after_release(self):
        with tempfile.TemporaryDirectory() as control_name:
            control = Path(control_name)
            control.chmod(0o700)
            with self.assertRaises(bootstrap.GateError) as raised:
                bootstrap.main(
                    [
                        "bootstrap.py",
                        str(control),
                        "120",
                        "--control-owner",
                        "501:0",
                        "--",
                        "agent",
                    ]
                )
            self.assertEqual(raised.exception.code, "argv-invalid")

            events = []

            def released_then_exec(*_args, **kwargs):
                self.assertNotIn("control_owner", kwargs)
                events.append("released")
                kwargs["exec_fn"]("agent", ["agent", "space value"], {"SAFE": "kept"})

            account = types.SimpleNamespace(
                pw_name="agent-user", pw_uid=1234, pw_gid=2345
            )
            with mock.patch.dict(os.environ, {}, clear=False), mock.patch.object(
                bootstrap, "run_gate", side_effect=released_then_exec
            ), mock.patch.object(
                bootstrap.Path, "read_text", return_value="fixture mountinfo"
            ), mock.patch.object(
                bootstrap.pwd, "getpwnam", return_value=account
            ), mock.patch.object(
                bootstrap.os,
                "initgroups",
                side_effect=lambda name, gid: events.append(("initgroups", name, gid)),
            ), mock.patch.object(
                bootstrap.os,
                "setgroups",
                side_effect=lambda groups: events.append(("setgroups", list(groups))),
            ), mock.patch.object(
                bootstrap.os, "setgid", side_effect=lambda gid: events.append(("setgid", gid))
            ), mock.patch.object(
                bootstrap.os, "setuid", side_effect=lambda uid: events.append(("setuid", uid))
            ), mock.patch.object(
                bootstrap.os, "geteuid", return_value=1234
            ), mock.patch.object(
                bootstrap.os, "getegid", return_value=3456
            ), mock.patch.object(
                bootstrap.os,
                "execvpe",
                side_effect=lambda file, argv, env: events.append(
                    ("exec", file, list(argv), dict(env))
                ),
            ):
                bootstrap.main(
                    [
                        "bootstrap.py",
                        str(control),
                        "120",
                        "--run-as",
                        "agent-user:3456",
                        "--container-name",
                        "fixture-container",
                        "--",
                        "agent",
                        "space value",
                    ]
                )

            self.assertEqual(
                events,
                [
                    "released",
                    ("setgroups", []),
                    ("setgid", 3456),
                    ("setuid", 1234),
                    ("exec", "agent", ["agent", "space value"], {"SAFE": "kept"}),
                ],
            )


class StartupGateRegressionTest(unittest.TestCase):
    VALID_MOUNTINFO = "11 1 0:2 / /review/input ro,relatime - bind /prepared ro"

    def test_verify_tree_rejects_root_symlink(self):
        with tempfile.TemporaryDirectory() as parent:
            parent_path = Path(parent)
            real = parent_path / "real"
            real.mkdir()
            link = parent_path / "root-link"
            link.symlink_to(real, target_is_directory=True)
            with self.assertRaises(bootstrap.GateError):
                bootstrap.verify_tree(link, {"version": 1, "entries": []})

    def test_verify_tree_rejects_walk_onerror_even_when_iterator_is_empty(self):
        with tempfile.TemporaryDirectory() as root:
            root_path = Path(root)
            def failed_walk(*_args, **kwargs):
                kwargs["onerror"](PermissionError("fixture traversal denied"))
                return iter(())
            with mock.patch.object(bootstrap.os, "walk", side_effect=failed_walk):
                with self.assertRaises(bootstrap.GateError):
                    bootstrap.verify_tree(root_path, {"version": 1, "entries": []})

    def test_read_write_probe_requires_successful_removal(self):
        with tempfile.TemporaryDirectory() as root:
            binding = {"workspace_path": root, "access": "read-write"}
            with mock.patch.object(Path, "unlink", side_effect=PermissionError("fixture unlink denied")):
                with self.assertRaises(bootstrap.GateError):
                    bootstrap.probe_access(binding)

    def test_run_gate_rejects_empty_bindings_without_ready_or_exec(self):
        calls = []
        with tempfile.TemporaryDirectory() as control:
            control_path = Path(control)
            (control_path / "request.json").write_bytes(b'{"version":1,"bindings":[]}')
            (control_path / "request.json").chmod(0o600)
            with self.assertRaises(bootstrap.GateError):
                bootstrap.run_gate(control_path, ["agent"], {}, self.VALID_MOUNTINFO, exec_fn=lambda *args: calls.append(args))
            self.assertFalse((control_path / "ready.json").exists())
        self.assertEqual(calls, [])

    def test_request_and_manifest_bounds_and_safe_manifest_basename_fail_before_ready(self):
        cases = ("request-oversize", "manifest-oversize", "traversal", "symlink")
        with tempfile.TemporaryDirectory() as owned:
            owned_path = Path(owned)
            outside = owned_path / "outside.json"
            outside.write_bytes(manifest_bytes([]))
            for case in cases:
                with self.subTest(case=case), tempfile.TemporaryDirectory(dir=owned) as control:
                    control_path = Path(control)
                    if case == "manifest-oversize":
                        raw = b'{"version":1,"entries":[]}' + b" " * (8 * 1024 * 1024 + 1 - len(b'{"version":1,"entries":[]}'))
                        manifest_file = "large.json"
                        (control_path / manifest_file).write_bytes(raw)
                    else:
                        raw = outside.read_bytes()
                        if case == "traversal":
                            manifest_file = "../outside.json"
                        elif case == "symlink":
                            manifest_file = "linked.json"
                            (control_path / manifest_file).symlink_to(outside)
                        else:
                            manifest_file = "review.json"
                            (control_path / manifest_file).write_bytes(raw)
                    request = {"version": 1, "bindings": [{"id": "review-input", "workspace_path": "/review/input", "manifest_id": hashlib.sha256(raw).hexdigest(), "manifest_file": manifest_file, "access": "read-only"}]}
                    request_raw = json.dumps(request, separators=(",", ":")).encode()
                    if case == "request-oversize":
                        request_raw += b" " * (4097 - len(request_raw))
                    (control_path / "request.json").write_bytes(request_raw)
                    (control_path / "request.json").chmod(0o600)
                    if (control_path / manifest_file).exists() and not (control_path / manifest_file).is_symlink():
                        (control_path / manifest_file).chmod(0o600)
                    expected_code = "request-invalid" if case in ("request-oversize", "traversal") else "manifest-invalid"
                    clock = FakeClock()
                    with mock.patch.object(bootstrap, "verify_tree", return_value=None), mock.patch.object(
                        bootstrap, "probe_access", return_value=None
                    ):
                        with self.assertRaises(bootstrap.GateError) as raised:
                            bootstrap.run_gate(
                                control_path,
                                ["agent"],
                                {},
                                self.VALID_MOUNTINFO,
                                exec_fn=lambda *_: self.fail("must not exec"),
                                timeout=1,
                                clock=clock.monotonic,
                                sleep=clock.sleep,
                            )
                    self.assertEqual(raised.exception.code, expected_code)
                    self.assertFalse((control_path / "ready.json").exists())

    def test_request_release_and_manifest_json_use_strict_records_and_exact_integers(self):
        nonce = "a" * 64
        binding = (
            '{"id":"review-input","workspace_path":"/review/input",'
            '"manifest_id":"' + "b" * 64 + '",'
            '"manifest_file":"review.manifest.json","access":"read-only"}'
        )
        invalid_requests = [
            ('{"version":2,"version":1,"bindings":[' + binding + ']}').encode(),
            ('{"version":1,"bindings":[' + binding.replace(
                '"id":"review-input"',
                '"id":"other","id":"review-input"',
            ) + ']}').encode(),
            ('{"version":true,"bindings":[' + binding + ']}').encode(),
            ('{"version":1.0,"bindings":[' + binding + ']}').encode(),
            ('{"version":1e0,"bindings":[' + binding + ']}').encode(),
            ('{"version":1,"bindings":[' + binding + '],"extra":false}').encode(),
            ('{"version":1,"bindings":[' + binding + ']} {}').encode(),
        ]
        for raw in invalid_requests:
            with self.subTest(record="request", raw=raw), tempfile.TemporaryDirectory() as control:
                control_path = Path(control)
                (control_path / "request.json").write_bytes(raw)
                (control_path / "request.json").chmod(0o600)
                with self.assertRaises(bootstrap.GateError) as raised:
                    bootstrap.run_gate(
                        control_path,
                        ["agent"],
                        {},
                        self.VALID_MOUNTINFO,
                        container_name="fixture-container",
                    )
                self.assertEqual(raised.exception.code, "request-invalid")
                self.assertFalse((control_path / "ready.json").exists())

        invalid_manifests = [
            b'{"version":2,"version":1,"entries":[]}',
            b'{"version":true,"entries":[]}',
            b'{"version":1.0,"entries":[]}',
            b'{"version":1e0,"entries":[]}',
            b'{"version":1,"entries":[{"path":"a","kind":"file","size":true,"sha256":"' + EMPTY_SHA.encode() + b'"}]}',
            b'{"version":1,"entries":[{"path":"a","kind":"file","size":1.0,"sha256":"' + EMPTY_SHA.encode() + b'"}]}',
            b'{"version":1,"entries":[{"path":"a","kind":"file","size":1e0,"sha256":"' + EMPTY_SHA.encode() + b'"}]}',
            b'{"version":1,"entries":[{"path":"a","kind":"directory","size":0.0,"sha256":null}]}',
            b'{"version":1,"entries":[{"path":"a","kind":"file","size":0,"size":0,"sha256":"' + EMPTY_SHA.encode() + b'"}]}',
            b'{"version":1,"entries":[],"extra":false}',
            b'{"version":1,"entries":[]} {}',
        ]
        for raw in invalid_manifests:
            with self.subTest(record="manifest", raw=raw):
                with self.assertRaises(bootstrap.GateError) as raised:
                    bootstrap.parse_manifest(raw, hashlib.sha256(raw).hexdigest())
                self.assertEqual(raised.exception.code, "manifest-invalid")

        invalid_releases = [
            ('{"version":1,"nonce":"' + "b" * 64 + '","nonce":"' + nonce + '"}').encode(),
            ('{"version":2,"version":1,"nonce":"' + nonce + '"}').encode(),
            ('{"version":true,"nonce":"' + nonce + '"}').encode(),
            ('{"version":1.0,"nonce":"' + nonce + '"}').encode(),
            ('{"version":1e0,"nonce":"' + nonce + '"}').encode(),
            ('{"version":1,"nonce":"' + nonce + '0"}').encode(),
            ('{"version":1,"nonce":"' + nonce + '","extra":false}').encode(),
            ('{"version":1,"nonce":"' + nonce + '"} {}').encode(),
        ]
        for raw in invalid_releases:
            with self.subTest(record="release", raw=raw), tempfile.TemporaryDirectory() as control:
                release = Path(control) / "release.json"
                release.write_bytes(raw)
                release.chmod(0o600)
                with self.assertRaises(bootstrap.GateError) as raised:
                    bootstrap.await_release(
                        Path(control),
                        {"version": 1, "nonce": nonce},
                        10,
                        clock=lambda: 0,
                        sleep=lambda _: None,
                    )
                self.assertEqual(raised.exception.code, "release-invalid")
                self.assertTrue(release.exists(), "invalid release must remain unconsumed")

    def test_release_deadline_is_checked_before_and_after_the_bounded_read(self):
        nonce = "a" * 64
        ready = {"version": 1, "nonce": nonce}

        class BoundaryClock:
            def __init__(self, after_start):
                self.calls = 0
                self.after_start = after_start

            def monotonic(self):
                self.calls += 1
                return 0.0 if self.calls == 1 else self.after_start

        with tempfile.TemporaryDirectory() as control:
            release = Path(control) / "release.json"
            write_protected_json(release, ready)
            clock = BoundaryClock(0.999)
            bootstrap.await_release(
                Path(control),
                ready,
                1,
                clock=clock.monotonic,
                sleep=lambda _: None,
            )
            self.assertFalse(release.exists(), "release before deadline is consumed")

        with tempfile.TemporaryDirectory() as control:
            release = Path(control) / "release.json"
            write_protected_json(release, ready)
            clock = BoundaryClock(1.0)
            with mock.patch.object(
                bootstrap,
                "_read_control_record",
                side_effect=AssertionError("deadline must be checked before open"),
            ) as reader:
                with self.assertRaises(bootstrap.GateError) as raised:
                    bootstrap.await_release(
                        Path(control),
                        ready,
                        1,
                        clock=clock.monotonic,
                        sleep=lambda _: None,
                    )
            self.assertEqual(raised.exception.code, "timeout")
            reader.assert_not_called()
            self.assertTrue(release.exists(), "release at deadline remains unconsumed")

        with tempfile.TemporaryDirectory() as control:
            release = Path(control) / "release.json"
            write_protected_json(release, ready)
            crossed = [False]
            original_read = bootstrap._read_control_record

            def clock():
                return 1.0 if crossed[0] else 0.0

            def read_then_cross(*args):
                raw = original_read(*args)
                crossed[0] = True
                return raw

            with mock.patch.object(
                bootstrap,
                "_read_control_record",
                side_effect=read_then_cross,
            ):
                with self.assertRaises(bootstrap.GateError) as raised:
                    bootstrap.await_release(
                        Path(control),
                        ready,
                        1,
                        clock=clock,
                        sleep=lambda _: None,
                    )
            self.assertEqual(raised.exception.code, "timeout")
            self.assertTrue(release.exists(), "release crossing deadline remains unconsumed")

    def test_main_resolves_named_and_numeric_runtime_identities_before_exact_drop(self):
        account = types.SimpleNamespace(
            pw_name="agent-user", pw_uid=1234, pw_gid=2345
        )
        named_group = types.SimpleNamespace(gr_name="agent-group", gr_gid=4567)

        def run_case(run_as, expected_events):
            events = []
            effective = {"uid": 0, "gid": 0}

            def released_then_exec(*_args, **kwargs):
                events.append("released")
                kwargs["exec_fn"]("agent", ["agent"], {"SAFE": "kept"})

            def setgroups(groups):
                events.append(("setgroups", list(groups)))

            def getgrouplist(name, gid):
                events.append(("getgrouplist", name, gid))
                return [gid, 5678]

            def setgid(gid):
                events.append(("setgid", gid))
                effective["gid"] = gid

            def setuid(uid):
                events.append(("setuid", uid))
                effective["uid"] = uid

            def getpwuid(uid):
                if uid == account.pw_uid:
                    raise KeyError(uid)
                raise AssertionError(f"unexpected uid lookup {uid}")

            fake_grp = types.SimpleNamespace(
                getgrnam=lambda name: named_group
                if name in (named_group.gr_name, "345x")
                else (_ for _ in ()).throw(KeyError(name))
            )
            with tempfile.TemporaryDirectory() as control_name:
                control = Path(control_name)
                control.chmod(0o700)
                with mock.patch.dict(os.environ, {}, clear=False), mock.patch.object(
                    bootstrap, "run_gate", side_effect=released_then_exec
                ), mock.patch.object(
                    bootstrap.Path, "read_text", return_value="fixture mountinfo"
                ), mock.patch.object(
                    bootstrap.Path, "exists", return_value=False
                ), mock.patch.object(
                    bootstrap.pwd, "getpwnam", return_value=account
                ), mock.patch.object(
                    bootstrap.pwd, "getpwuid", side_effect=getpwuid
                ), mock.patch.object(
                    bootstrap, "grp", fake_grp, create=True
                ), mock.patch.object(
                    bootstrap.os, "setgroups", side_effect=setgroups
                ), mock.patch.object(
                    bootstrap.os, "getgrouplist", side_effect=getgrouplist
                ), mock.patch.object(
                    bootstrap.os,
                    "initgroups",
                    side_effect=AssertionError(
                        "runtime identity groups were re-resolved after readiness"
                    ),
                ), mock.patch.object(
                    bootstrap.os, "setgid", side_effect=setgid
                ), mock.patch.object(
                    bootstrap.os, "setuid", side_effect=setuid
                ), mock.patch.object(
                    bootstrap.os, "geteuid", side_effect=lambda: effective["uid"]
                ) as read_uid, mock.patch.object(
                    bootstrap.os, "getegid", side_effect=lambda: effective["gid"]
                ) as read_gid, mock.patch.object(
                    bootstrap.os,
                    "execvpe",
                    side_effect=lambda file, argv, env: events.append(
                        ("exec", file, list(argv), dict(env))
                    ),
                ):
                    bootstrap.main(
                        [
                            "bootstrap.py",
                            str(control),
                            "120",
                            "--run-as",
                            run_as,
                            "--container-name",
                            "fixture-container",
                            "--",
                            "agent",
                        ]
                    )
            read_uid.assert_called_once_with()
            read_gid.assert_called_once_with()
            self.assertEqual(events, expected_events)

        run_case(
            "agent-user",
            [
                ("getgrouplist", "agent-user", 2345),
                "released",
                ("setgroups", [2345, 5678]),
                ("setgid", 2345),
                ("setuid", 1234),
                ("exec", "agent", ["agent"], {"SAFE": "kept"}),
            ],
        )
        run_case(
            "1234:3456",
            [
                "released",
                ("setgroups", []),
                ("setgid", 3456),
                ("setuid", 1234),
                ("exec", "agent", ["agent"], {"SAFE": "kept"}),
            ],
        )
        run_case(
            "1234:agent-group",
            [
                "released",
                ("setgroups", []),
                ("setgid", 4567),
                ("setuid", 1234),
                ("exec", "agent", ["agent"], {"SAFE": "kept"}),
            ],
        )
        run_case(
            "agent-user:agent-group",
            [
                "released",
                ("setgroups", []),
                ("setgid", 4567),
                ("setuid", 1234),
                ("exec", "agent", ["agent"], {"SAFE": "kept"}),
            ],
        )
        run_case(
            "123x:345x",
            [
                "released",
                ("setgroups", []),
                ("setgid", 4567),
                ("setuid", 1234),
                ("exec", "agent", ["agent"], {"SAFE": "kept"}),
            ],
        )

    def test_main_rejects_unknown_named_user_or_group_without_exec(self):
        account = types.SimpleNamespace(
            pw_name="agent-user", pw_uid=1234, pw_gid=2345
        )

        def released_then_exec(*_args, **kwargs):
            kwargs["exec_fn"]("agent", ["agent"], {})

        for run_as in ("unknown-user:3456", "agent-user:unknown-group"):
            with self.subTest(run_as=run_as), tempfile.TemporaryDirectory() as control_name:
                control = Path(control_name)
                control.chmod(0o700)
                def getpwnam(name):
                    if run_as.startswith("agent-user:") and name == "agent-user":
                        return account
                    raise KeyError(name)

                fake_grp = types.SimpleNamespace(
                    getgrnam=mock.Mock(side_effect=KeyError("unknown-group"))
                )
                with mock.patch.dict(os.environ, {}, clear=False), mock.patch.object(
                    bootstrap, "run_gate", side_effect=released_then_exec
                ), mock.patch.object(
                    bootstrap.Path, "read_text", return_value="fixture mountinfo"
                ), mock.patch.object(
                    bootstrap.Path, "exists", return_value=False
                ), mock.patch.object(
                    bootstrap.pwd,
                    "getpwnam",
                    side_effect=getpwnam,
                ), mock.patch.object(
                    bootstrap, "grp", fake_grp, create=True
                ), mock.patch.object(
                    bootstrap.os,
                    "initgroups",
                    side_effect=AssertionError("unresolved identity changed supplementary groups"),
                ), mock.patch.object(
                    bootstrap.os,
                    "setgroups",
                    side_effect=AssertionError("unresolved identity changed supplementary groups"),
                ), mock.patch.object(
                    bootstrap.os,
                    "setgid",
                    side_effect=AssertionError("unresolved identity changed gid"),
                ), mock.patch.object(
                    bootstrap.os,
                    "setuid",
                    side_effect=AssertionError("unresolved identity changed uid"),
                ), mock.patch.object(
                    bootstrap.os,
                    "execvpe",
                    side_effect=AssertionError("invalid identity executed agent"),
                ):
                    with self.assertRaises(bootstrap.GateError) as raised:
                        bootstrap.main(
                            [
                                "bootstrap.py",
                                str(control),
                                "120",
                                "--run-as",
                                run_as,
                                "--container-name",
                                "fixture-container",
                                "--",
                                "agent",
                            ]
                        )
                self.assertEqual(raised.exception.code, "runtime-user")

    def test_run_gate_verifies_every_binding_as_resolved_identity_before_ready(self):
        with mock.patch.object(
            bootstrap.pwd, "getpwuid", side_effect=KeyError(1234)
        ):
            identity = bootstrap.resolve_runtime_identity("1234:3456")
        events = []
        real_atomic = bootstrap._atomic_control_json

        def record_atomic(control, name, value):
            events.append(("record", name))
            return real_atomic(control, name, value)

        with tempfile.TemporaryDirectory() as control_name:
            control = Path(control_name)
            control.chmod(0o700)
            write_valid_control(control)
            with mock.patch.object(
                bootstrap,
                "validate_mount",
                side_effect=lambda *_args: events.append(("mount",)),
            ), mock.patch.object(
                bootstrap,
                "verify_binding_as",
                side_effect=lambda _binding, _manifest, actual: events.append(
                    ("verify-as", actual)
                ),
            ), mock.patch.object(
                bootstrap,
                "await_release",
                side_effect=lambda *_args, **_kwargs: events.append(("release",)),
            ), mock.patch.object(
                bootstrap,
                "_atomic_control_json",
                side_effect=record_atomic,
            ):
                bootstrap.run_gate(
                    control,
                    ["agent", "--exact"],
                    {"SAFE": "kept"},
                    "fixture mountinfo",
                    runtime_identity=identity,
                    container_name="fixture-container",
                    exec_fn=lambda *args: events.append(("exec", args)),
                )

        verify_index = events.index(("verify-as", identity))
        ready_index = events.index(("record", "ready.json"))
        self.assertLess(verify_index, ready_index)
        self.assertEqual(
            [event for event in events if event[0] == "verify-as"],
            [("verify-as", identity)],
        )
        self.assertEqual(events[-1][0], "exec")

    def test_runtime_identity_is_an_immutable_tuple_record(self):
        identity = bootstrap.RuntimeIdentity(
            uid=1234,
            gid=3456,
            groups=(3456, 4567),
            drop=True,
        )

        self.assertIsInstance(identity, tuple)
        self.assertEqual(identity._fields, ("uid", "gid", "groups", "drop"))
        self.assertEqual(identity.groups, (3456, 4567))
        with self.assertRaises(AttributeError):
            identity.uid = 9999
        with self.assertRaises(TypeError):
            identity.groups[0] = 9999

        with mock.patch.object(bootstrap.pwd, "getpwuid", side_effect=KeyError(1234)):
            resolved = bootstrap.resolve_runtime_identity("1234:3456")
        self.assertIsInstance(resolved, bootstrap.RuntimeIdentity)
        self.assertEqual(
            resolved,
            bootstrap.RuntimeIdentity(1234, 3456, (), True),
        )

    def test_verify_binding_as_closes_both_pipe_ends_when_fork_fails(self):
        close = mock.Mock()
        with mock.patch.object(
            bootstrap.os, "pipe", return_value=(101, 102)
        ), mock.patch.object(
            bootstrap.os, "fork", side_effect=OSError("fork failed")
        ), mock.patch.object(
            bootstrap.os, "close", close
        ), mock.patch.object(
            bootstrap.os,
            "kill",
            side_effect=AssertionError("no child exists to kill"),
        ), mock.patch.object(
            bootstrap.os,
            "waitpid",
            side_effect=AssertionError("no child exists to reap"),
        ):
            with self.assertRaises(bootstrap.GateError) as raised:
                bootstrap.verify_binding_as({}, {}, object())

        self.assertEqual(raised.exception.code, "identity-probe")
        self.assertCountEqual(close.call_args_list, [mock.call(101), mock.call(102)])

    def test_verify_binding_as_closes_pipe_ends_and_preserves_fork_interruption(self):
        close = mock.Mock()
        with mock.patch.object(
            bootstrap.os, "pipe", return_value=(111, 112)
        ), mock.patch.object(
            bootstrap.os, "fork", side_effect=KeyboardInterrupt("stop")
        ), mock.patch.object(
            bootstrap.os, "close", close
        ), mock.patch.object(
            bootstrap.os,
            "kill",
            side_effect=AssertionError("no child exists to kill"),
        ), mock.patch.object(
            bootstrap.os,
            "waitpid",
            side_effect=AssertionError("no child exists to reap"),
        ):
            with self.assertRaises(KeyboardInterrupt):
                bootstrap.verify_binding_as({}, {}, object())

        self.assertCountEqual(close.call_args_list, [mock.call(111), mock.call(112)])

    def test_verify_binding_as_kills_and_reaps_child_when_wait_fails(self):
        child_pid = 4242
        wait_calls = []

        def wait_then_reap(pid, options):
            wait_calls.append((pid, options))
            if len(wait_calls) == 1:
                raise OSError("wait failed")
            return (pid, 0)

        close = mock.Mock()
        kill = mock.Mock()
        with mock.patch.object(
            bootstrap.os, "pipe", return_value=(201, 202)
        ), mock.patch.object(
            bootstrap.os, "fork", return_value=child_pid
        ), mock.patch.object(
            bootstrap.os, "close", close
        ), mock.patch.object(
            bootstrap.os, "waitpid", side_effect=wait_then_reap
        ), mock.patch.object(
            bootstrap.os, "kill", kill
        ), mock.patch.object(
            bootstrap.os,
            "read",
            side_effect=AssertionError("a failed wait cannot consume child output"),
        ):
            with self.assertRaises(bootstrap.GateError) as raised:
                bootstrap.verify_binding_as({}, {}, object())

        self.assertEqual(raised.exception.code, "identity-probe")
        self.assertEqual(
            wait_calls,
            [(child_pid, os.WNOHANG), (child_pid, 0)],
        )
        kill.assert_called_once_with(child_pid, 9)
        self.assertCountEqual(close.call_args_list, [mock.call(201), mock.call(202)])

    def test_verify_binding_as_reaps_child_and_preserves_wait_interruption(self):
        child_pid = 4343
        wait_calls = []

        def interrupt_then_reap(pid, options):
            wait_calls.append((pid, options))
            if len(wait_calls) == 1:
                raise KeyboardInterrupt("stop")
            return (pid, 0)

        close = mock.Mock()
        kill = mock.Mock()
        with mock.patch.object(
            bootstrap.os, "pipe", return_value=(211, 212)
        ), mock.patch.object(
            bootstrap.os, "fork", return_value=child_pid
        ), mock.patch.object(
            bootstrap.os, "close", close
        ), mock.patch.object(
            bootstrap.os, "waitpid", side_effect=interrupt_then_reap
        ), mock.patch.object(
            bootstrap.os, "kill", kill
        ), mock.patch.object(
            bootstrap.os,
            "read",
            side_effect=AssertionError("an interrupted wait cannot consume child output"),
        ):
            with self.assertRaises(KeyboardInterrupt):
                bootstrap.verify_binding_as({}, {}, object())

        self.assertEqual(
            wait_calls,
            [(child_pid, os.WNOHANG), (child_pid, 0)],
        )
        kill.assert_called_once_with(child_pid, 9)
        self.assertCountEqual(close.call_args_list, [mock.call(211), mock.call(212)])

    def test_main_resolves_runtime_identity_once_before_entering_gate(self):
        identity = object()
        with tempfile.TemporaryDirectory() as control_name:
            control = Path(control_name)
            control.chmod(0o700)
            with mock.patch.dict(os.environ, {}, clear=False), mock.patch.object(
                bootstrap,
                "resolve_runtime_identity",
                return_value=identity,
            ) as resolve, mock.patch.object(
                bootstrap, "run_gate", return_value=None
            ) as run_gate, mock.patch.object(
                bootstrap.Path, "read_text", return_value="fixture mountinfo"
            ), mock.patch.object(
                bootstrap.Path, "exists", return_value=False
            ):
                bootstrap.main(
                    [
                        "bootstrap.py",
                        str(control),
                        "120",
                        "--run-as",
                        "1234:3456",
                        "--container-name",
                        "fixture-container",
                        "--",
                        "agent",
                    ]
                )

        resolve.assert_called_once_with("1234:3456")
        self.assertIs(
            run_gate.call_args.kwargs["runtime_identity"],
            identity,
            "the gate and final exec must share the one resolved identity",
        )

    @unittest.skipUnless(
        hasattr(os, "fork") and os.geteuid() == 0,
        "real target-identity access check requires a root-owned bounded child",
    )
    def test_target_identity_cannot_certify_root_only_workspace(self):
        with tempfile.TemporaryDirectory() as root_name:
            root = Path(root_name)
            root.chmod(0o700)
            secret = root / "secret.txt"
            secret.write_bytes(b"root only")
            secret.chmod(0o600)
            manifest = {
                "version": 1,
                "entries": [
                    {
                        "path": "secret.txt",
                        "kind": "file",
                        "size": len(b"root only"),
                        "sha256": hashlib.sha256(b"root only").hexdigest(),
                    }
                ],
            }
            binding = {
                "id": "root-only",
                "workspace_path": str(root),
                "access": "read-only",
            }
            with mock.patch.object(
                bootstrap.pwd, "getpwuid", side_effect=KeyError(65534)
            ):
                identity = bootstrap.resolve_runtime_identity("65534:65534")

            with self.assertRaises(bootstrap.GateError) as raised:
                bootstrap.verify_binding_as(binding, manifest, identity)
            self.assertEqual(raised.exception.code, "tree-io")


if __name__ == "__main__":
    unittest.main()
class IdentityProbeCleanupRegressionTests(unittest.TestCase):
    def test_pipe_oserror_is_identity_probe_without_claiming_child_ownership(self):
        with mock.patch.object(
            bootstrap.os, "pipe", side_effect=OSError("pipe failed")
        ), mock.patch.object(
            bootstrap.os, "fork", side_effect=AssertionError("no child")
        ), mock.patch.object(
            bootstrap.os, "close", side_effect=AssertionError("no descriptors")
        ), mock.patch.object(
            bootstrap.os, "kill", side_effect=AssertionError("no child")
        ), mock.patch.object(
            bootstrap.os, "waitpid", side_effect=AssertionError("no child")
        ):
            with self.assertRaises(bootstrap.GateError) as raised:
                bootstrap.verify_binding_as({}, {}, object())

        self.assertEqual(raised.exception.code, "identity-probe")

    def test_pipe_baseexception_propagates_without_claiming_child_ownership(self):
        with mock.patch.object(
            bootstrap.os, "pipe", side_effect=KeyboardInterrupt("stop")
        ), mock.patch.object(
            bootstrap.os, "fork", side_effect=AssertionError("no child")
        ), mock.patch.object(
            bootstrap.os, "close", side_effect=AssertionError("no descriptors")
        ), mock.patch.object(
            bootstrap.os, "kill", side_effect=AssertionError("no child")
        ), mock.patch.object(
            bootstrap.os, "waitpid", side_effect=AssertionError("no child")
        ):
            with self.assertRaises(KeyboardInterrupt):
                bootstrap.verify_binding_as({}, {}, object())

    def test_cleanup_retries_eintr_and_reaps_only_the_owned_child(self):
        child_pid = 4444
        wait_calls = []

        def fail_then_interrupt_then_reap(pid, options):
            wait_calls.append((pid, options))
            if len(wait_calls) == 1:
                raise OSError("initial wait failed")
            if len(wait_calls) == 2:
                raise InterruptedError("cleanup interrupted")
            return (pid, 0)

        close = mock.Mock()
        kill = mock.Mock()
        with mock.patch.object(
            bootstrap.os, "pipe", return_value=(221, 222)
        ), mock.patch.object(
            bootstrap.os, "fork", return_value=child_pid
        ), mock.patch.object(
            bootstrap.os, "close", close
        ), mock.patch.object(
            bootstrap.os, "waitpid", side_effect=fail_then_interrupt_then_reap
        ), mock.patch.object(
            bootstrap.os, "kill", kill
        ), mock.patch.object(
            bootstrap.os, "read", side_effect=AssertionError("no read")
        ):
            with self.assertRaises(bootstrap.GateError) as raised:
                bootstrap.verify_binding_as({}, {}, object())

        self.assertEqual(raised.exception.code, "identity-probe")
        kill.assert_called_once_with(child_pid, 9)
        self.assertEqual(
            wait_calls,
            [(child_pid, os.WNOHANG), (child_pid, 0), (child_pid, 0)],
        )
        self.assertCountEqual(close.call_args_list, [mock.call(221), mock.call(222)])

    def test_cleanup_accepts_childprocesserror_as_exact_child_already_reaped(self):
        child_pid = 4545
        wait_calls = []

        def fail_then_already_reaped(pid, options):
            wait_calls.append((pid, options))
            if len(wait_calls) == 1:
                raise OSError("initial wait failed")
            raise ChildProcessError("already reaped")

        close = mock.Mock()
        kill = mock.Mock()
        with mock.patch.object(
            bootstrap.os, "pipe", return_value=(231, 232)
        ), mock.patch.object(
            bootstrap.os, "fork", return_value=child_pid
        ), mock.patch.object(
            bootstrap.os, "close", close
        ), mock.patch.object(
            bootstrap.os, "waitpid", side_effect=fail_then_already_reaped
        ), mock.patch.object(
            bootstrap.os, "kill", kill
        ), mock.patch.object(
            bootstrap.os, "read", side_effect=AssertionError("no read")
        ):
            with self.assertRaises(bootstrap.GateError) as raised:
                bootstrap.verify_binding_as({}, {}, object())

        self.assertEqual(raised.exception.code, "identity-probe")
        kill.assert_called_once_with(child_pid, 9)
        self.assertEqual(wait_calls, [(child_pid, os.WNOHANG), (child_pid, 0)])
        self.assertCountEqual(close.call_args_list, [mock.call(231), mock.call(232)])

    def test_read_failure_does_not_kill_an_already_reaped_child(self):
        child_pid = 4646
        wait_calls = []

        def running_then_reap(pid, options):
            wait_calls.append((pid, options))
            if len(wait_calls) == 1:
                return (0, 0)
            return (pid, 0)

        close = mock.Mock()
        kill = mock.Mock()
        with mock.patch.object(
            bootstrap.os, "pipe", return_value=(241, 242)
        ), mock.patch.object(
            bootstrap.os, "fork", return_value=child_pid
        ), mock.patch.object(
            bootstrap.os, "close", close
        ), mock.patch.object(
            bootstrap.os, "waitpid", side_effect=running_then_reap
        ), mock.patch.object(
            bootstrap.time, "sleep"
        ), mock.patch.object(
            bootstrap.os, "kill", kill
        ), mock.patch.object(
            bootstrap.os, "read", side_effect=OSError("read failed")
        ):
            with self.assertRaises(bootstrap.GateError) as raised:
                bootstrap.verify_binding_as({}, {}, object())

        self.assertEqual(raised.exception.code, "identity-probe")
        kill.assert_not_called()
        self.assertEqual(
            wait_calls,
            [(child_pid, os.WNOHANG), (child_pid, os.WNOHANG)],
        )
        self.assertCountEqual(close.call_args_list, [mock.call(241), mock.call(242)])
