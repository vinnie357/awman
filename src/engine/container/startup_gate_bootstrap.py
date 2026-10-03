#!/usr/bin/python3
"""Fixed, dependency-free awman startup-gate bootstrap."""

import errno, hashlib, json, os, pwd, secrets, stat, sys, time
from pathlib import Path

class GateError(Exception):
    def __init__(self, code, message=None):
        self.code = code
        super().__init__(message or code)

def _fail(code, message):
    raise GateError(code, message)

def _json(raw, allowed, code):
    try:
        value = json.loads(raw.decode("utf-8"))
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
    if value["version"] != 1 or not isinstance(value["entries"], list):
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
        if entry["kind"] == "directory":
            if entry["size"] != 0 or entry["sha256"] is not None: _fail("manifest-invalid", "invalid directory")
        elif entry["kind"] == "file":
            if not isinstance(entry["size"], int) or entry["size"] < 0 or not _hex64(entry["sha256"]): _fail("manifest-invalid", "invalid file")
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

def await_release(control_dir, ready, timeout, cancel_check=lambda: False, clock=time.monotonic, sleep=time.sleep, control_owner=None):
    control_dir = Path(control_dir); start = clock(); release_path = control_dir / "release.json"
    while True:
        if cancel_check(): _fail("cancelled", "startup gate cancelled")
        try:
            release_present = release_path.lstat() is not None
        except FileNotFoundError:
            release_present = False
        except OSError as exc:
            _fail("release-invalid", str(exc))
        if release_present:
            owner = control_owner[0] if control_owner is not None else None
            mode = 0o600 if control_owner is not None else None
            raw = _read_bounded_regular(release_path, 4096, "release-invalid", owner, mode)
            release = _json(raw, ("version", "nonce"), "release-invalid")
            if release["version"] != 1 or release["nonce"] != ready["nonce"]: _fail("release-invalid", "release does not match ready nonce")
            release_path.unlink(); return
        if clock() - start >= timeout: _fail("timeout", "startup gate timeout")
        sleep(0.05)

def _read_bounded_regular(path, maximum, code, protected_owner=None, protected_mode=None):
    try:
        info = path.lstat()
        if not stat.S_ISREG(info.st_mode) or stat.S_ISLNK(info.st_mode) or info.st_nlink != 1 or info.st_size > maximum:
            _fail(code, "unsafe or oversized file")
        if protected_owner is not None and info.st_uid != protected_owner:
            _fail(code, "unexpected file owner")
        if protected_mode is not None and stat.S_IMODE(info.st_mode) != protected_mode:
            _fail(code, "unexpected file mode")
        flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0)
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

def run_gate(control_dir, original_argv, original_env, mountinfo_text, cancel_check=lambda: False, clock=time.monotonic, sleep=time.sleep, exec_fn=os.execvpe, timeout=120, container_name=None, approved_dir=None, control_owner=None):
    control_dir = Path(control_dir)
    approved_dir = Path(approved_dir) if approved_dir is not None else control_dir
    try:
        request = _load_request(approved_dir, protected=approved_dir != control_dir)
        verified = []
        for binding in request["bindings"]:
            validate_mount(binding, mountinfo_text)
            manifest_path = approved_dir / binding["manifest_file"]
            owner = approved_dir.lstat().st_uid if approved_dir != control_dir else None
            mode = 0o600 if approved_dir != control_dir else None
            raw = _read_bounded_regular(manifest_path, 8 * 1024 * 1024, "manifest-invalid", owner, mode)
            manifest = parse_manifest(raw, binding["manifest_id"])
            verify_tree(Path(binding["workspace_path"]), manifest)
            probe_access(binding)
            verified.append({k: binding[k] for k in ("id", "manifest_id", "access", "workspace_path")})
        if not container_name: _fail("identity-invalid", "missing authoritative container identity")
        ready = {"version": 1, "nonce": secrets.token_hex(32), "container_name": container_name, "bindings": verified}
        _atomic_json(control_dir / "ready.json", ready, control_owner)
        print("AWMAN_STARTUP_GATE_READY " + ready["nonce"], file=sys.stderr, flush=True)
        await_release(control_dir, ready, timeout, cancel_check=cancel_check, clock=clock, sleep=sleep, control_owner=control_owner)
        _atomic_json(control_dir / ".released", {"version": 1, "nonce": ready["nonce"]}, control_owner)
        print("AWMAN_STARTUP_GATE_RELEASED " + ready["nonce"], file=sys.stderr, flush=True)
        exec_fn(original_argv[0], original_argv, original_env)
    except GateError as exc:
        _atomic_json(control_dir / "failure.json", {"version": 1, "code": exc.code, "message": str(exc)}, control_owner)
        print("AWMAN_STARTUP_GATE_FAILED " + exc.code, file=sys.stderr, flush=True)
        raise
    except Exception as exc:
        wrapped = GateError("internal-error", str(exc))
        _atomic_json(control_dir / "failure.json", {"version": 1, "code": wrapped.code, "message": str(wrapped)}, control_owner)
        print("AWMAN_STARTUP_GATE_FAILED " + wrapped.code, file=sys.stderr, flush=True)
        raise wrapped

def main(argv):
    if "--" not in argv: _fail("argv-invalid", "missing argv boundary")
    boundary = argv.index("--")
    if boundary < 3 or len(argv) <= boundary + 1: _fail("argv-invalid", "invalid bootstrap arguments")
    control, timeout = Path(argv[1]), int(argv[2])
    run_as = None
    container_name = None
    control_owner = None
    index = 3
    while index < boundary:
        if index + 1 >= boundary: _fail("argv-invalid", "missing bootstrap option value")
        if argv[index] == "--run-as": run_as = argv[index + 1]
        elif argv[index] == "--container-name": container_name = argv[index + 1]
        elif argv[index] == "--control-owner":
            uid, separator, gid = argv[index + 1].partition(":")
            if not separator or not uid.isdigit() or not gid.isdigit(): _fail("argv-invalid", "invalid control owner")
            control_owner = (int(uid), int(gid))
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
    def final_exec(file, args, env):
        if run_as is not None:
            user_name, _, group_text = run_as.partition(":")
            try:
                record = pwd.getpwuid(int(user_name)) if user_name.isdigit() else pwd.getpwnam(user_name)
                gid = int(group_text) if group_text.isdigit() else record.pw_gid
                os.initgroups(record.pw_name, gid)
                os.setgid(gid)
                os.setuid(record.pw_uid)
            except (KeyError, ValueError, OSError) as exc: _fail("runtime-user", str(exc))
        os.execvpe(file, args, env)
    run_gate(control, original, original_env, mountinfo, timeout=timeout, approved_dir=Path(__file__).resolve().parent, exec_fn=final_exec, control_owner=control_owner, container_name=container_name)

if __name__ == "__main__":
    try: main(sys.argv)
    except GateError: sys.exit(70)
