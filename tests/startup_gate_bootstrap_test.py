"""WI 0118 fixed-bootstrap tests. No container, network, or model is used."""

import hashlib
import importlib.util
import json
import os
from pathlib import Path
import tempfile
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
            (control_path / "release.json").write_text(
                json.dumps({"version": 1, "nonce": ready["nonce"]}), encoding="utf-8"
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

            (control_path / "release.json").write_text(
                json.dumps({"version": 1, "nonce": "b" * 64}), encoding="utf-8"
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
                    exec_fn=capture_exec,
                )

        self.assertEqual(events, ["mount", "tree", "probe", "ready", "release", "exec"])
        self.assertEqual(exec_calls, [(original_argv[0], original_argv, original_env)])


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
