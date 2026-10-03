"""WI 0118 fixed-bootstrap tests. No container, network, or model is used."""

import hashlib
import importlib.util
import json
import os
from pathlib import Path
import stat
import subprocess
import sys
import tempfile
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


class StartupGateBootstrapTest(unittest.TestCase):
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

        self.assertEqual(events, ["mount", "tree", "probe", "ready", "release", "exec"])
        self.assertEqual(exec_calls, [(original_argv[0], original_argv, original_env)])


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
                bootstrap.os, "setgid", side_effect=lambda gid: events.append(("setgid", gid))
            ), mock.patch.object(
                bootstrap.os, "setuid", side_effect=lambda uid: events.append(("setuid", uid))
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
                    ("initgroups", "agent-user", 3456),
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


if __name__ == "__main__":
    unittest.main()
