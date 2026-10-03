#!/usr/bin/python3
"""Fixed, dependency-free awman startup-gate bootstrap."""

import errno, grp, hashlib, json, os, pwd, secrets, stat, sys, time
from collections import namedtuple
from pathlib import Path

RuntimeIdentity = namedtuple("RuntimeIdentity", ("uid", "gid", "groups", "drop"))

class GateError(Exception):
    def __init__(self, code, message=None):
        self.code = code
        super().__init__(message or code)

class ControlDirectory:
    def __init__(self, fd, uid, gid):
        self.fd = fd
        self.uid = uid
        self.gid = gid

    def close(self):
        if self.fd >= 0:
            os.close(self.fd)
            self.fd = -1

def _validate_control_handle(control):
    try:
        info = os.fstat(control.fd)
    except OSError as exc:
        _fail("control-invalid", str(exc))
    if (not stat.S_ISDIR(info.st_mode) or stat.S_IMODE(info.st_mode) != 0o700
            or info.st_uid != control.uid or info.st_gid != control.gid):
        _fail("control-invalid", "unsafe held control directory")
    return info

def open_control_directory(path):
    flags = os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0)
    fd = None
    try:
        before = Path(path).lstat()
        fd = os.open(path, flags)
        info = os.fstat(fd)
    except OSError as exc:
        if fd is not None: os.close(fd)
        _fail("control-invalid", str(exc))
    if ((before.st_dev, before.st_ino) != (info.st_dev, info.st_ino)
            or not stat.S_ISDIR(info.st_mode) or stat.S_IMODE(info.st_mode) != 0o700):
        os.close(fd)
        _fail("control-invalid", "unsafe control directory")
    return ControlDirectory(fd, info.st_uid, info.st_gid)

def _fail(code, message):
    raise GateError(code, message)

def _json(raw, allowed, code):
    def strict_object(pairs):
        value = {}
        for key, item in pairs:
            if key in value:
                _fail(code, "duplicate object field")
            value[key] = item
        return value
    try:
        value = json.loads(raw.decode("utf-8"), object_pairs_hook=strict_object)
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        _fail(code, str(exc))
    if not isinstance(value, dict) or set(value) != set(allowed):
        _fail(code, "invalid object fields")
    return value

def _hex64(value):
    return isinstance(value, str) and len(value) == 64 and all(c in "0123456789abcdef" for c in value)

def parse_manifest(raw_bytes, expected_digest):
    if raw_bytes.startswith(b"\xef\xbb\xbf") or hashlib.sha256(raw_bytes).hexdigest() != expected_digest:
        _fail("manifest-invalid", "manifest identity mismatch")
    value = _json(raw_bytes, ("version", "entries"), "manifest-invalid")
    if type(value["version"]) is not int or value["version"] != 1 or not isinstance(value["entries"], list):
        _fail("manifest-invalid", "unsupported manifest")
    previous = None
    for entry in value["entries"]:
        if not isinstance(entry, dict) or set(entry) != {"path", "kind", "size", "sha256"}:
            _fail("manifest-invalid", "invalid entry fields")
        path = entry["path"]
        if not isinstance(path, str) or not path or path.startswith("/") or "\\" in path or "\0" in path or any(p in ("", ".", "..") for p in path.split("/")):
            _fail("manifest-invalid", "invalid entry path")
        encoded = path.encode("utf-8")
        if previous is not None and previous >= encoded:
            _fail("manifest-invalid", "entries not strictly sorted")
        previous = encoded
        if type(entry["size"]) is not int or entry["size"] < 0:
            _fail("manifest-invalid", "invalid entry size")
        if entry["kind"] == "directory":
            if entry["size"] != 0 or entry["sha256"] is not None: _fail("manifest-invalid", "invalid directory")
        elif entry["kind"] == "file":
            if not _hex64(entry["sha256"]): _fail("manifest-invalid", "invalid file")
        else: _fail("manifest-invalid", "invalid kind")
    return value

def _unescape_mount(value):
    for old, new in (("\\040", " "), ("\\011", "\t"), ("\\012", "\n"), ("\\134", "\\")):
        value = value.replace(old, new)
    return value

def validate_mount(binding, mountinfo_text):
    root = binding["workspace_path"].rstrip("/") or "/"
    mounts = []
    for line in mountinfo_text.splitlines():
        left, sep, _right = line.partition(" - ")
        fields = left.split()
        if not sep or len(fields) < 6: continue
        mount = _unescape_mount(fields[4])
        opts = fields[5].split(",")
        mounts.append((mount, opts))
    covering = [(m, o) for m, o in mounts if root == m or root.startswith(m.rstrip("/") + "/")]
    if not covering: _fail("mount-invalid", "workspace has no covering mount")
    effective, options = max(covering, key=lambda item: len(item[0]))
    expected = "ro" if binding["access"] == "read-only" else "rw"
    if expected not in options: _fail("mount-access", "effective mount access differs")
    for mount, _ in mounts:
        if mount != root and mount.startswith(root + "/"):
            _fail("nested-mount", "unexpected nested mount")
    return effective

def verify_tree(root_path, manifest):
    root = Path(root_path)
    actual = []
    try:
        root_info = root.lstat()
    except OSError as exc:
        _fail("tree-io", str(exc))
    if not stat.S_ISDIR(root_info.st_mode) or stat.S_ISLNK(root_info.st_mode):
        _fail("tree-root", "workspace root is not a nonsymlink directory")
    traversal_error = []
    def onerror(exc):
        traversal_error.append(exc)
    try:
        for current, dirs, files in os.walk(root, topdown=True, followlinks=False, onerror=onerror):
            if traversal_error: _fail("tree-io", str(traversal_error[0]))
            dirs.sort(key=lambda s: os.fsencode(s)); files.sort(key=lambda s: os.fsencode(s))
            for name in dirs + files:
                path = Path(current) / name
                rel = path.relative_to(root).as_posix()
                info = path.lstat()
                if stat.S_ISLNK(info.st_mode): _fail("tree-link", "symlink rejected")
                if stat.S_ISDIR(info.st_mode): actual.append({"path": rel, "kind": "directory", "size": 0, "sha256": None})
                elif stat.S_ISREG(info.st_mode):
                    if info.st_nlink != 1: _fail("tree-link", "hard-linked file rejected")
                    digest = hashlib.sha256()
                    flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0)
                    fd = os.open(path, flags)
                    opened = os.fstat(fd)
                    if (opened.st_dev, opened.st_ino, opened.st_mode, opened.st_size, opened.st_nlink) != (info.st_dev, info.st_ino, info.st_mode, info.st_size, info.st_nlink):
                        os.close(fd); _fail("tree-race", "file changed while opening")
                    with os.fdopen(fd, "rb") as stream:
                        for block in iter(lambda: stream.read(1024 * 1024), b""): digest.update(block)
                    actual.append({"path": rel, "kind": "file", "size": info.st_size, "sha256": digest.hexdigest()})
                else: _fail("tree-special", "special file rejected")
        if traversal_error: _fail("tree-io", str(traversal_error[0]))
    except OSError as exc: _fail("tree-io", str(exc))
    actual.sort(key=lambda e: e["path"].encode("utf-8"))
    if actual != manifest["entries"]: _fail("tree-mismatch", "workspace differs from manifest")

def probe_access(binding):
    root = Path(binding["workspace_path"])
    probe = root / (".awman-startup-probe-" + secrets.token_hex(16))
    try:
        fd = os.open(probe, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    except OSError as exc:
        if binding["access"] == "read-only" and exc.errno in (errno.EACCES, errno.EROFS): return
        _fail("access-probe", str(exc))
    else:
        try:
            os.write(fd, b"probe"); os.fsync(fd)
        finally:
            os.close(fd)
            try: probe.unlink()
            except OSError as exc: _fail("access-probe", str(exc))
        if binding["access"] == "read-only": _fail("access-probe", "read-only binding was writable")

def resolve_runtime_identity(run_as):
    if run_as is None:
        return RuntimeIdentity(os.geteuid(), os.getegid(), tuple(os.getgroups()), False)
    user_text, separator, group_text = run_as.partition(":")
    try:
        if user_text.isdigit():
            uid = int(user_text)
            try: record = pwd.getpwuid(uid)
            except KeyError: record = None
        else:
            record = pwd.getpwnam(user_text)
            uid = record.pw_uid
        if separator:
            if not group_text: _fail("runtime-user", "empty runtime group")
            gid = int(group_text) if group_text.isdigit() else grp.getgrnam(group_text).gr_gid
            groups = ()
        else:
            if record is None:
                _fail("runtime-user", "numeric user without passwd entry requires an explicit group")
            gid = record.pw_gid
            groups = tuple(dict.fromkeys(os.getgrouplist(record.pw_name, gid)))
        return RuntimeIdentity(uid, gid, groups, True)
    except (KeyError, ValueError, OSError) as exc:
        _fail("runtime-user", str(exc))

def apply_runtime_identity(identity):
    if not identity.drop:
        return
    try:
        os.setgroups(list(identity.groups))
        os.setgid(identity.gid)
        os.setuid(identity.uid)
        if os.geteuid() != identity.uid or os.getegid() != identity.gid:
            _fail("runtime-user", "effective identity differs after restoration")
    except (KeyError, ValueError, OSError) as exc:
        _fail("runtime-user", str(exc))

def verify_binding_as(binding, manifest, identity):
    try:
        read_fd, write_fd = os.pipe()
    except BaseException as exc:
        if isinstance(exc, (KeyboardInterrupt, SystemExit)):
            raise
        _fail("identity-probe", str(exc))
    try:
        pid = os.fork()
    except BaseException as exc:
        try: os.close(read_fd)
        except OSError: pass
        try: os.close(write_fd)
        except OSError: pass
        if isinstance(exc, (KeyboardInterrupt, SystemExit)):
            raise
        _fail("identity-probe", str(exc))
    if pid == 0:
        status = 1
        try:
            try:
                os.close(read_fd)
                apply_runtime_identity(identity)
                verify_tree(Path(binding["workspace_path"]), manifest)
                probe_access(binding)
                payload = b""
                status = 0
            except GateError as exc:
                payload = (exc.code + "\0" + str(exc)).encode("utf-8", "replace")[:4096]
            except BaseException as exc:
                payload = ("identity-probe\0" + str(exc)).encode("utf-8", "replace")[:4096]
            try:
                if payload: os.write(write_fd, payload)
            except BaseException:
                status = 1
        finally:
            try: os.close(write_fd)
            finally: os._exit(status)
    deadline = time.monotonic() + 30
    reaped = False
    write_open = True
    try:
        os.close(write_fd)
        write_open = False
        while True:
            waited, status = os.waitpid(pid, os.WNOHANG)
            if waited == pid:
                reaped = True
                break
            if time.monotonic() >= deadline:
                _fail("identity-probe", "target-identity verification timed out")
            time.sleep(0.01)
        payload = os.read(read_fd, 4097)
    except BaseException as exc:
        if not reaped:
            try: os.kill(pid, 9)
            except OSError: pass
            while True:
                try:
                    waited, _ = os.waitpid(pid, 0)
                except InterruptedError:
                    continue
                except ChildProcessError:
                    reaped = True
                    break
                except OSError:
                    break
                else:
                    if waited == pid:
                        reaped = True
                    break
        if isinstance(exc, (GateError, KeyboardInterrupt, SystemExit)):
            raise
        _fail("identity-probe", str(exc))
    finally:
        if write_open:
            try: os.close(write_fd)
            except OSError: pass
        try: os.close(read_fd)
        except OSError: pass
    if len(payload) > 4096:
        _fail("identity-probe", "target-identity verification response too large")
    if not os.WIFEXITED(status) or os.WEXITSTATUS(status) != 0:
        if payload:
            code, _, message = payload.decode("utf-8", "replace").partition("\0")
            _fail(code or "identity-probe", message or "target-identity verification failed")
        _fail("identity-probe", "target-identity verification failed")

def _atomic_json(path, value, owner=None):
    temp = path.with_name("." + path.name + "." + secrets.token_hex(8))
    payload = json.dumps(value, separators=(",", ":"), sort_keys=True).encode("utf-8")
    fd = os.open(temp, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    try: os.write(fd, payload); os.fsync(fd)
    finally: os.close(fd)
    if owner is not None:
        owner_fd = os.open(temp, os.O_RDONLY)
        try: os.fchown(owner_fd, owner[0], owner[1])
        finally: os.close(owner_fd)
    os.replace(temp, path)
    directory = os.open(path.parent, os.O_RDONLY)
    try: os.fsync(directory)
    finally: os.close(directory)

def _read_control_record(control, name, maximum, code):
    _validate_control_handle(control)
    if not isinstance(name, str) or not name or "/" in name or "\\" in name or name in (".", ".."):
        _fail(code, "invalid control record name")
    fd = None
    try:
        before = os.stat(name, dir_fd=control.fd, follow_symlinks=False)
        if (not stat.S_ISREG(before.st_mode) or before.st_nlink != 1
                or stat.S_IMODE(before.st_mode) != 0o600
                or before.st_uid != control.uid or before.st_gid != control.gid
                or before.st_size == 0 or before.st_size > maximum):
            _fail(code, "unsafe or oversized file")
        flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_NONBLOCK", 0)
        fd = os.open(name, flags, dir_fd=control.fd)
        opened = os.fstat(fd)
        identity = lambda value: (value.st_dev, value.st_ino, value.st_mode, value.st_size, value.st_nlink, value.st_uid, value.st_gid)
        if identity(before) != identity(opened) or not stat.S_ISREG(opened.st_mode):
            os.close(fd); fd = None; _fail(code, "file changed while opening")
        with os.fdopen(fd, "rb") as stream:
            fd = None
            raw = stream.read(maximum + 1)
            after = os.fstat(stream.fileno())
            if identity(opened) != identity(after): _fail(code, "file changed while reading")
        if not raw or len(raw) > maximum: _fail(code, "file too large")
        return raw
    except GateError:
        if fd is not None: os.close(fd)
        raise
    except OSError as exc:
        if fd is not None: os.close(fd)
        _fail(code, str(exc))

def _atomic_control_json(control, name, value):
    _validate_control_handle(control)
    payload = json.dumps(value, separators=(",", ":"), sort_keys=True).encode("utf-8")
    temp = "." + name + "." + secrets.token_hex(8)
    fd = None
    try:
        fd = os.open(temp, os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0), 0o600, dir_fd=control.fd)
        offset = 0
        while offset < len(payload):
            written = os.write(fd, payload[offset:])
            if written <= 0: _fail("control-write", "short control record write")
            offset += written
        os.fchmod(fd, 0o600)
        try:
            os.fchown(fd, control.uid, control.gid)
        except PermissionError:
            current = os.fstat(fd)
            if current.st_uid != control.uid or current.st_gid != control.gid:
                raise
        info = os.fstat(fd)
        if (not stat.S_ISREG(info.st_mode) or info.st_nlink != 1
                or stat.S_IMODE(info.st_mode) != 0o600
                or info.st_uid != control.uid or info.st_gid != control.gid):
            _fail("control-write", "unsafe created control record")
        os.fsync(fd)
        os.close(fd); fd = None
        os.replace(temp, name, src_dir_fd=control.fd, dst_dir_fd=control.fd)
        os.fsync(control.fd)
    except Exception:
        if fd is not None: os.close(fd)
        try: os.unlink(temp, dir_fd=control.fd)
        except OSError: pass
        raise

def _unlink_control_record(control, name, code):
    _validate_control_handle(control)
    try:
        os.unlink(name, dir_fd=control.fd)
        os.fsync(control.fd)
    except OSError as exc:
        _fail(code, str(exc))

def await_release(control_dir, ready, timeout, cancel_check=lambda: False, clock=time.monotonic, sleep=time.sleep, control_handle=None, control_owner=None):
    del control_owner
    control = control_handle or open_control_directory(control_dir)
    owns_control = control_handle is None
    start = clock()
    try:
        while True:
            if cancel_check(): _fail("cancelled", "startup gate cancelled")
            if clock() - start >= timeout: _fail("timeout", "startup gate timeout")
            try:
                os.stat("release.json", dir_fd=control.fd, follow_symlinks=False)
                release_present = True
            except FileNotFoundError:
                release_present = False
            except OSError as exc:
                _fail("release-invalid", str(exc))
            if release_present:
                raw = _read_control_record(control, "release.json", 4096, "release-invalid")
                release = _json(raw, ("version", "nonce"), "release-invalid")
                if type(release["version"]) is not int or release["version"] != 1 or not _hex64(release["nonce"]) or release["nonce"] != ready["nonce"]: _fail("release-invalid", "release does not match ready nonce")
                if clock() - start >= timeout: _fail("timeout", "startup gate timeout")
                _unlink_control_record(control, "release.json", "release-invalid")
                return
            sleep(0.05)
    finally:
        if owns_control: control.close()

def _read_bounded_regular(path, maximum, code, protected_owner=None, protected_mode=None):
    try:
        info = path.lstat()
        if not stat.S_ISREG(info.st_mode) or stat.S_ISLNK(info.st_mode) or info.st_nlink != 1 or info.st_size > maximum:
            _fail(code, "unsafe or oversized file")
        if protected_owner is not None and info.st_uid != protected_owner:
            _fail(code, "unexpected file owner")
        if protected_mode is not None and stat.S_IMODE(info.st_mode) != protected_mode:
            _fail(code, "unexpected file mode")
        flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_NONBLOCK", 0)
        fd = os.open(path, flags)
        opened = os.fstat(fd)
        if (opened.st_dev, opened.st_ino, opened.st_mode, opened.st_size, opened.st_nlink) != (info.st_dev, info.st_ino, info.st_mode, info.st_size, info.st_nlink):
            os.close(fd); _fail(code, "file changed while opening")
        with os.fdopen(fd, "rb") as stream: raw = stream.read(maximum + 1)
        if len(raw) > maximum: _fail(code, "file too large")
        return raw
    except GateError:
        raise
    except OSError as exc:
        _fail(code, str(exc))

def _load_request(control_dir, protected=False):
    owner = None
    mode = None
    if protected:
        try:
            directory = control_dir.lstat()
        except OSError as exc:
            _fail("request-invalid", str(exc))
        if not stat.S_ISDIR(directory.st_mode) or stat.S_ISLNK(directory.st_mode) or stat.S_IMODE(directory.st_mode) != 0o700:
            _fail("request-invalid", "unsafe approved directory")
        owner = directory.st_uid
        mode = 0o600
    raw = _read_bounded_regular(control_dir / "request.json", 4096, "request-invalid", owner, mode)
    request = _json(raw, ("version", "bindings"), "request-invalid")
    if type(request["version"]) is not int or request["version"] != 1 or not isinstance(request["bindings"], list) or not request["bindings"]:
        _fail("request-invalid", "unsupported request")
    ids, paths = set(), []
    for binding in request["bindings"]:
        if not isinstance(binding, dict) or set(binding) != {"id", "workspace_path", "manifest_id", "manifest_file", "access"}: _fail("request-invalid", "invalid binding fields")
        if not isinstance(binding["id"], str) or not binding["id"] or binding["id"] in ids: _fail("request-invalid", "invalid binding id")
        ids.add(binding["id"])
        path = binding["workspace_path"]
        if not isinstance(path, str) or not path.startswith("/") or path.endswith("/") or any(part in ("", ".", "..") for part in path.split("/")[1:]): _fail("request-invalid", "invalid workspace path")
        if any(path == prior or path.startswith(prior + "/") or prior.startswith(path + "/") for prior in paths): _fail("request-invalid", "overlapping workspace paths")
        paths.append(path)
        name = binding["manifest_file"]
        reserved = {"bootstrap.py", "request.json", "original-python-env.json", ".released", "ready.json", "release.json", "failure.json"}
        if not isinstance(name, str) or not name or "/" in name or "\\" in name or name in reserved or name in (".", ".."): _fail("request-invalid", "invalid manifest basename")
        if binding["access"] not in ("read-only", "read-write") or not _hex64(binding["manifest_id"]): _fail("request-invalid", "invalid binding")
    return request

def run_gate(control_dir, original_argv, original_env, mountinfo_text, cancel_check=lambda: False, clock=time.monotonic, sleep=time.sleep, exec_fn=os.execvpe, timeout=120, container_name=None, approved_dir=None, control_handle=None, control_owner=None, runtime_identity=None):
    del control_owner
    control_dir = Path(control_dir)
    approved_dir = Path(approved_dir) if approved_dir is not None else control_dir
    control = control_handle or open_control_directory(control_dir)
    owns_control = control_handle is None
    try:
        _validate_control_handle(control)
        request = _load_request(approved_dir, protected=approved_dir != control_dir)
        verified = []
        for binding in request["bindings"]:
            validate_mount(binding, mountinfo_text)
            manifest_path = approved_dir / binding["manifest_file"]
            owner = approved_dir.lstat().st_uid if approved_dir != control_dir else None
            mode = 0o600 if approved_dir != control_dir else None
            raw = _read_bounded_regular(manifest_path, 8 * 1024 * 1024, "manifest-invalid", owner, mode)
            manifest = parse_manifest(raw, binding["manifest_id"])
            if runtime_identity is None:
                verify_tree(Path(binding["workspace_path"]), manifest)
                probe_access(binding)
            else:
                verify_binding_as(binding, manifest, runtime_identity)
            verified.append({k: binding[k] for k in ("id", "manifest_id", "access", "workspace_path")})
        if not container_name: _fail("identity-invalid", "missing authoritative container identity")
        ready = {"version": 1, "nonce": secrets.token_hex(32), "container_name": container_name, "bindings": verified}
        _atomic_control_json(control, "ready.json", ready)
        print("AWMAN_STARTUP_GATE_READY " + ready["nonce"], file=sys.stderr, flush=True)
        await_release(control_dir, ready, timeout, cancel_check=cancel_check, clock=clock, sleep=sleep, control_handle=control)
        _atomic_control_json(control, ".released", {"version": 1, "nonce": ready["nonce"]})
        print("AWMAN_STARTUP_GATE_RELEASED " + ready["nonce"], file=sys.stderr, flush=True)
        exec_fn(original_argv[0], original_argv, original_env)
    except GateError as exc:
        _atomic_control_json(control, "failure.json", {"version": 1, "code": exc.code, "message": str(exc)})
        print("AWMAN_STARTUP_GATE_FAILED " + exc.code, file=sys.stderr, flush=True)
        raise
    except Exception as exc:
        wrapped = GateError("internal-error", str(exc))
        _atomic_control_json(control, "failure.json", {"version": 1, "code": wrapped.code, "message": str(wrapped)})
        print("AWMAN_STARTUP_GATE_FAILED " + wrapped.code, file=sys.stderr, flush=True)
        raise wrapped
    finally:
        if owns_control: control.close()

def main(argv):
    if "--" not in argv: _fail("argv-invalid", "missing argv boundary")
    boundary = argv.index("--")
    if boundary < 3 or len(argv) <= boundary + 1: _fail("argv-invalid", "invalid bootstrap arguments")
    control, timeout = Path(argv[1]), int(argv[2])
    run_as = None
    container_name = None
    index = 3
    while index < boundary:
        if index + 1 >= boundary: _fail("argv-invalid", "missing bootstrap option value")
        if argv[index] == "--run-as": run_as = argv[index + 1]
        elif argv[index] == "--container-name": container_name = argv[index + 1]
        else: _fail("argv-invalid", "unknown bootstrap option")
        index += 2
    original = argv[boundary + 1:]
    original_env = dict(os.environ)
    preserved_path = Path(__file__).resolve().parent / "original-python-env.json"
    if preserved_path.exists():
        try: preserved = json.loads(preserved_path.read_bytes().decode("utf-8"))
        except (UnicodeDecodeError, json.JSONDecodeError) as exc: _fail("environment-invalid", str(exc))
        if not isinstance(preserved, dict) or not set(preserved).issubset({"PYTHONHOME", "PYTHONPATH"}): _fail("environment-invalid", "invalid preserved Python environment fields")
        if not all(key in ("PYTHONHOME", "PYTHONPATH") and isinstance(value, str) for key, value in preserved.items()): _fail("environment-invalid", "invalid preserved Python environment")
        original_env.update(preserved)
    os.environ.pop("PYTHONHOME", None); os.environ.pop("PYTHONPATH", None)
    mountinfo = Path("/proc/self/mountinfo").read_text(encoding="utf-8")
    runtime_identity = resolve_runtime_identity(run_as)
    def final_exec(file, args, env):
        apply_runtime_identity(runtime_identity)
        os.execvpe(file, args, env)
    run_gate(control, original, original_env, mountinfo, timeout=timeout, approved_dir=Path(__file__).resolve().parent, exec_fn=final_exec, container_name=container_name, runtime_identity=runtime_identity)

if __name__ == "__main__":
    try: main(sys.argv)
    except GateError: sys.exit(70)
