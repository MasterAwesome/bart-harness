import ast
import base64
import builtins
import collections
import collections.abc
import codecs
import functools
import itertools
import io
import importlib
import json
import linecache
import os
import pathlib
import re
import signal
import shlex
import site
import stat
import sys
import threading
import textwrap
import time
import traceback
import types
import warnings
import weakref

if sys.version_info < (3, 10):
    sys.exit(f"Bart needs Python 3.10 or newer; python3 is {sys.version.split()[0]}")

def _import_paths(original, project, pythonpath, user_site):
    # Keep the standard library first; don't execute user-site .pth startup code.
    boundary = next((i for i, entry in enumerate(original)
                     if entry.endswith(("site-packages", "dist-packages"))), len(original))
    extra = [project, *(os.path.abspath(entry) for entry in pythonpath.split(os.pathsep) if entry)]
    if user_site:
        extra.append(user_site)
    return list(dict.fromkeys([*original[:boundary], *extra, *original[boundary:]]))


sys.pycache_prefix = sys.argv[2]
_user_site = None
if (sys.prefix == sys.base_prefix and not os.environ.get("PYTHONNOUSERSITE")
        and (not hasattr(os, "geteuid") or (os.geteuid() == os.getuid() and os.getegid() == os.getgid()))):
    _user_site = site.getusersitepackages()
sys.path[:] = _import_paths(sys.path, os.getcwd(), os.environ.get("PYTHONPATH", ""), _user_site)

MARKER = sys.argv[1].encode()
_remaining_barrier = 0

requests = os.fdopen(os.dup(0), "rb")
_CONFIG = json.loads(sys.argv[3])
MAX_REQUEST_BYTES = _CONFIG["request_bytes"]
_request_failed = False
replies = os.fdopen(os.dup(1), "w", encoding="utf-8")
with open(os.devnull, "rb") as null:
    os.dup2(null.fileno(), 0)
os.dup2(2, 1)
sys.stdout = os.fdopen(1, "w", buffering=1, encoding="utf-8", errors="replace", closefd=False)
sys.stderr = os.fdopen(2, "w", buffering=1, encoding="utf-8", errors="replace", closefd=False)
_diagnostics = os.fdopen(2, "w", buffering=1, encoding="utf-8", errors="replace", closefd=False)


def _diagnostic(text=""):
    for stream in (sys.stdout, sys.stderr):
        try:
            stream.flush()
        except BaseException:
            pass
    _diagnostics.write(text)
    if text and not text.endswith("\n"):
        _diagnostics.write("\n")
    _diagnostics.flush()


class BartError(Exception):
    """A helper Bart refused or could not carry out."""


_channel = threading.Lock()
_in_cell = False
_activity_cell = 0
# Delay SIGINT until the exchange ends, or the next request could receive this reply.
_exchanging = None
_interrupted = False


def _on_interrupt(signum, frame):
    global _interrupted, _exchanging
    if _exchanging is not None:
        _interrupted = True
        if _exchanging == "reading":
            _exchanging = "interrupted"
            os.write(replies.fileno(), b'{"interrupt":{}}\n')
    elif _in_cell:
        _interrupted = _channel.locked()
        raise KeyboardInterrupt


signal.signal(signal.SIGINT, _on_interrupt)


def _read_line():
    global _request_failed
    if _request_failed:
        raise BartError("interpreter request channel has failed")
    line = requests.readline(MAX_REQUEST_BYTES + 1)
    if len(line) > MAX_REQUEST_BYTES or (line and not line.endswith(b"\n")):
        _request_failed = True
        raise BartError("interpreter request exceeds its byte limit or is incomplete")
    try:
        return line.decode("utf-8")
    except UnicodeDecodeError:
        _request_failed = True
        raise


def _exchange(message, *, activity_cell=None):
    global _exchanging, _interrupted
    main = threading.current_thread() is threading.main_thread()
    with _channel:
        if activity_cell is not None and (not _in_cell or activity_cell != _activity_cell):
            return {"result": None}
        if main and _interrupted:
            _interrupted = False
            raise KeyboardInterrupt
        _exchanging = "writing"
        try:
            try:
                encoded = json.dumps(message, allow_nan=False)
            except (TypeError, ValueError) as error:
                raise BartError(f"arguments are not JSON-serializable: {error}") from None
            replies.write(encoded + "\n")
            replies.flush()
            _exchanging = "reading"
            if _interrupted:
                _on_interrupt(signal.SIGINT, None)
            reply = json.loads(_read_line())
        finally:
            _exchanging = None
    if main and _interrupted:
        _interrupted = False
        raise KeyboardInterrupt
    return reply


def _activity_message(message, cell):
    reply = _exchange({"activity": message}, activity_cell=cell)
    if "error" in reply:
        raise BartError(reply["error"])
    return reply["result"]


def _activity(title, invoke, outcome=lambda value: "completed"):
    cell = _activity_cell
    ident = _activity_message({"start": {"title": title[:512]}}, cell)
    try:
        value = invoke()
    except BaseException as error:
        try:
            if ident is not None:
                _activity_message({"finish": {"id": ident, "outcome":
                    "interrupted" if isinstance(error, KeyboardInterrupt) else "failed"}}, cell)
        except (OSError, ValueError, BartError, KeyboardInterrupt):
            pass  # Preserve the original error; Rust settles outstanding rows on cell exit.
        raise
    if ident is not None:
        _activity_message({"finish": {"id": ident, "outcome": outcome(value)}}, cell)
    return value


def _call(name, **arguments):
    def invoke():
        reply = _exchange({"call": {"name": name, "arguments": arguments}})
        if "error" in reply:
            raise BartError(reply["error"])
        return reply["result"]
    # Search and image delivery are reported by Rust. Job snapshots are internal accessors.
    if name in ("web_search", "image", "job", "remaining"):
        return invoke()
    if name in ("run", "background"):
        argv = arguments["argv"]
        command = shlex.join(argv) if all(type(arg) is str for arg in argv) else "invalid argv"
        title = f"{'run' if name == 'run' else 'bg_run'}({command})"
    elif name.startswith("mcp"):
        public = "mcp" if name == "mcp_call" else name.replace("mcp_", "mcp.", 1)
        target = arguments.get("server") or ""
        if "tool" in arguments:
            target += "/" + arguments["tool"]
        elif arguments.get("query") is not None:
            target += f", query={arguments['query']!r}" if target else f"query={arguments['query']!r}"
        title = f"{public}({target})"
    elif name == "kill":
        title = f"job[{arguments['job']}].kill()"
    else:
        title = f"{name}()"
    def outcome(value):
        if name == "run" and (value.get("return_code") != 0 or value.get("timed_out")):
            return "failed"
        if name == "mcp_call" and value.get("error") is not None:
            return "failed"
        return "completed"
    return _activity(title, invoke, outcome)


class Text(str):
    """Text from Bart, shown as text rather than as a quoted string."""

    def __repr__(self):
        return str(self)


class _SavedPath(type(pathlib.Path())):
    reference: str


class SavedStream:
    def __init__(self, path, incomplete, reference):
        self.path = _SavedPath(path)
        self.path.reference = reference
        self.incomplete = incomplete
        self._reference = reference

    def __repr__(self):
        return f"SavedStream(path={self._reference}, incomplete={self.incomplete!r})"


_RUN_ERROR_VIEW_BYTES = 2 * 1024


def _bart(kind, fields):
    fields = str(fields).replace("\n", " ").replace("\r", " ")
    return f"[Bart {kind}: {fields}]"


class Result:
    """A command result with an attributed display and complete stream accessors."""

    def __init__(self, return_code, outcome, timed_out, cleanup_error, retained_job, stdout_file, stderr_file, output_file, reference, state):
        self.return_code = return_code
        self.stdout_file = SavedStream(**stdout_file, reference=reference.replace(".run[", ".stdout[") if reference.startswith("cell[") else f"{reference}.stdout_file.path")
        self.stderr_file = SavedStream(**stderr_file, reference=reference.replace(".run[", ".stderr[") if reference.startswith("cell[") else f"{reference}.stderr_file.path")
        self.outcome = outcome
        self.timed_out = timed_out
        self.cleanup_error = cleanup_error
        self.retained_job = retained_job
        self._output = output_file
        self._reference = reference
        self._state = state

    def _snapshot(self):
        return self

    @staticmethod
    def _complete_text(stream, name):
        if stream.incomplete is not None:
            raise BartError(f"{name} capture is incomplete: {stream.incomplete}; inspect {name}_file.path for partial bytes")
        try:
            with stream.path.open("rb") as file:
                data = file.read(64 * 1024 * 1024 + 1)
            if len(data) > 64 * 1024 * 1024:
                raise BartError(f"{name} capture exceeds 64 MiB; inspect {name}_file.path")
            return data.decode("utf-8")
        except UnicodeDecodeError:
            raise BartError(f"{name} is not UTF-8; use {name}_file.path.read_bytes() for exact bytes") from None
        except OSError as error:
            raise BartError(f"cannot read {name} capture: {_reason(error)}; inspect its saved reference") from None

    @property
    def stdout(self):
        return self._complete_text(self._snapshot().stdout_file, "stdout")

    @property
    def stderr(self):
        return self._complete_text(self._snapshot().stderr_file, "stderr")

    def __repr__(self):
        return "".join(self._view_parts())

    def _heading(self, flags=()):
        if (self._output["completeness"] != "complete"
                or self.stdout_file.incomplete is not None or self.stderr_file.incomplete is not None):
            flags = [*flags, "incomplete capture"]
        return _bart("job" if self._reference.startswith("job[") else "run",
                     "; ".join([f"{self._reference} {self._state}", *flags]))

    def _view_parts(self):
        result = self._snapshot()
        try:
            file = pathlib.Path(result._output["path"]).open("rb")
        except OSError as error:
            yield result._heading(["output unavailable"]) + "\n" + _reason(error)
            return
        with file:
            yield result._heading()
            remaining = os.fstat(file.fileno()).st_size
            decoder = codecs.getincrementaldecoder("utf-8")(errors="backslashreplace")
            if remaining:
                yield "\n"
            while remaining:
                part = file.read(min(_CHUNK, remaining))
                if not part:
                    break
                remaining -= len(part)
                yield decoder.decode(part)
            yield decoder.decode(b"", final=True)

    def _display(self):
        for part in self._view_parts():
            sys.stdout.write(part)
        sys.stdout.write("\n")

    def _view(self):
        result = self._snapshot()
        flags = []
        try:
            with pathlib.Path(result._output["path"]).open("rb") as file:
                size = os.fstat(file.fileno()).st_size
                start = max(0, size - _RUN_ERROR_VIEW_BYTES)
                file.seek(start)
                data = file.read(_RUN_ERROR_VIEW_BYTES)
            if start:
                while data and data[0] & 0xC0 == 0x80:
                    data = data[1:]
            if size > len(data):
                flags.append(f"last {len(data)} of {size} bytes shown")
            text = data.decode("utf-8", errors="backslashreplace").rstrip("\n")
        except OSError as error:
            flags.append("output unavailable")
            text = _reason(error)
        return result._heading(flags) + ("\n" + text if text else "")

    def __bool__(self):
        code = self._snapshot().return_code
        if code is None:
            raise BartError("command has not finished; use job events rather than polling")
        return code == 0


class RunError(BartError):
    def __init__(self, result):
        self.result = result
        super().__init__("\n" + result._view())


def _argv(argv):
    if isinstance(argv, (str, bytes)) or not isinstance(argv, collections.abc.Sequence):
        raise BartError("argv must be a nonempty sequence of strings")
    return list(argv)


def _cwd(cwd):
    if cwd is None:
        return None
    path = os.fspath(cwd)
    if not isinstance(path, str) or not path or "\0" in path:
        raise BartError("cwd must be a nonempty text path without NUL")
    return path


def _run(argv, timeout, check, **options):
    if not isinstance(check, bool):
        raise BartError("check must be True or False")
    try:
        result = Result(**_call("run", argv=_argv(argv), timeout=timeout, **options))
    finally:
        importlib.invalidate_caches()
    if (result.timed_out or not isinstance(result.outcome, dict)
            or not ({"exit", "signal"} & result.outcome.keys())
            or (check and result.return_code != 0)):
        raise RunError(result)
    return result


def run(argv, *, timeout=_CONFIG["process_default_seconds"], check=True, cwd=None, env=None):
    return _run(argv, timeout, check, cwd=_cwd(cwd), env={} if env is None else dict(env))


def run_escaping(argv, *, timeout=_CONFIG["process_default_seconds"], check=True, cwd=None, env=None, sandbox=True, reason=None):
    return _run(argv, timeout, check, cwd=_cwd(cwd), env={} if env is None else dict(env), sandbox=sandbox, reason=reason)


class Job(Result):
    def __init__(self, id, command):
        self.id = id
        self.command = command

    def _saved_path(self):
        return _saved(f"job-{self.id}.log", f"job[{self.id}]")

    def __fspath__(self):
        return os.fspath(self._saved_path())

    def _snapshot(self):
        return Result(**_call("job", id=self.id))

    def __getattr__(self, name):
        if name in ("return_code", "outcome", "stdout_file", "stderr_file", "timed_out", "cleanup_error", "retained_job"):
            return getattr(self._snapshot(), name)
        raise AttributeError(name)

    if _CONFIG["one_turn"]:
        def wait(self):
            while self._snapshot().outcome is None:
                time.sleep(0.2)
            return self

    def kill(self):
        """Stop this job; completed effects are not rolled back."""
        result = _call("kill", job=self.id)
        if result is not None:
            _changed[result] = None
            print(result)


def _job(number):
    _saved(f"job-{number}.log", f"job[{number}]")
    return Job(number, "")


_SAVED = pathlib.Path(_CONFIG["saved_dir"])


def _saved(name, label):
    path = _SavedPath(_SAVED / name)
    path.reference = label
    if not path.is_file():
        raise BartError(f"{label} does not exist in this session")
    return path


class _Numbered(collections.abc.Mapping):
    """Saved files of one kind by number, listed from the session's directory on each use."""

    def __init__(self, label, prefix, value):
        self._label, self._prefix, self._value = label, prefix, value

    def _numbers(self):
        prefix = self._prefix
        numbers = set()
        for name in os.listdir(_SAVED):
            stem = name.split(".", 1)[0]
            if stem.startswith(prefix) and stem[len(prefix):].isdigit():
                numbers.add(int(stem[len(prefix):]))
        return sorted(numbers)

    def __getitem__(self, number):
        if type(number) is not int or number < 1:
            raise BartError(f"{self._label}[N] takes a positive number")
        return self._value(number)

    def __iter__(self):
        return iter(self._numbers())

    def __len__(self):
        return len(self._numbers())

    def __repr__(self):
        numbers = self._numbers()
        shown = f"{numbers[0]}..{numbers[-1]}" if numbers else "none yet"
        return f"{self._label}[{shown}]"


class _Remaining(Text):
    def __new__(cls, text, notice):
        value = super().__new__(cls, text)
        value.notice = notice
        return value

    def __str__(self):
        text = str.__str__(self)
        return text + ("\n" if text and not text.endswith("\n") else "") + self.notice


class Cell:
    __slots__ = ("_number",)

    def __init__(self, number):
        self._number = number

    def __repr__(self):
        return f"cell[{self._number}]"

    def remaining(self):
        global _remaining_barrier
        _diagnostic()
        _remaining_barrier += 1
        os.write(2, b"\0" + MARKER + b":remaining\0")
        return _Remaining(**_call("remaining", cell=self._number, barrier=_remaining_barrier))

    @property
    def code(self):
        return _saved(f"code-{self._number}.py", f"cell[{self._number}].code")

    @property
    def output(self):
        return _saved(f"output-{self._number}.log", f"cell[{self._number}].output")

    @property
    def status(self):
        return _saved(f"status-{self._number}.jsonl", f"cell[{self._number}].status")

    @property
    def stdout(self):
        number = self._number
        return _Numbered(f"cell[{number}].stdout", f"run-{number}-",
                         lambda call: _saved(f"run-{number}-{call}.stdout", f"cell[{number}].stdout[{call}]"))

    @property
    def stderr(self):
        number = self._number
        return _Numbered(f"cell[{number}].stderr", f"run-{number}-",
                         lambda call: _saved(f"run-{number}-{call}.stderr", f"cell[{number}].stderr[{call}]"))

    @property
    def run(self):
        cell = self._number
        return _Numbered(f"cell[{cell}].run", f"run-{cell}-",
                         lambda call: _saved(f"run-{cell}-{call}.log", f"cell[{cell}].run[{call}]"))


def _cell(number):
    _saved(f"output-{number}.log", f"cell[{number}]")
    return Cell(number)


def _image(number):
    for path in _SAVED.glob(f"image-{number}.*"):
        return path
    raise BartError(f"image[{number}] does not exist in this session")


job = _Numbered("job", "job-", _job)
cell = _Numbered("cell", "output-", _cell)
image = _Numbered("image", "image-", _image)
paste = _Numbered("paste", "paste-", lambda number: _saved(f"paste-{number}.txt", f"paste[{number}]"))
history = _Numbered("history", "history-", lambda number: _saved(f"history-{number}.json", f"history[{number}]"))


def whoami():
    """Current configured provider, model and reasoning effort; queries the host on each call."""
    return _call("whoami")


def _background(argv, notify, env, **options):
    result = Job(**_call("background", argv=_argv(argv), notify=notify,
                        env={} if env is None else dict(env), **options))
    _diagnostic(_bart("job", f"job[{result.id}] started"))
    return result


def bg_run(argv, *, notify=None, cwd=None, env=None):
    """Start one program; outcomes arrive as events."""
    return _background(argv, notify, env, cwd=_cwd(cwd))


def bg_run_escaping(argv, *, notify=None, cwd=None, env=None, sandbox=True, reason=None):
    """bg_run with an explicit request to leave the sandbox."""
    return _background(argv, notify, env, cwd=_cwd(cwd), sandbox=sandbox, reason=reason)


run_escaping.__name__ = run_escaping.__qualname__ = "run"
bg_run_escaping.__name__ = bg_run_escaping.__qualname__ = "bg_run"
if _CONFIG["escape"]:
    run, bg_run = run_escaping, bg_run_escaping


def web_search(*queries):
    """Search for each query and return its answer and sources."""
    if not all(isinstance(query, str) for query in queries):
        raise BartError("web_search queries must be strings")
    return Text(_call("web_search", queries=queries))


MAX_FILE_BYTES = _CONFIG["file_bytes"]
MAX_DIFF_BYTES = _CONFIG["diff_bytes"]
MAX_SCAN_BYTES = _CONFIG["scan_bytes"]
MAX_EDITS = _CONFIG["edits"]
MAX_REPLACEMENTS = _CONFIG["replacements"]
MAX_MATCH_LINES = _CONFIG["match_lines"]
MAX_IMAGE_BYTES = _CONFIG["image_bytes"]
_CHUNK = _CONFIG["chunk_bytes"]
_changed = {}


def _path(path):
    path = os.fspath(path)
    if not isinstance(path, str) or not path:
        raise BartError("the path must be a non-empty string")
    return path


def _count(value, name):
    if isinstance(value, bool) or not isinstance(value, int) or value < 0:
        raise BartError(f"{name} must be a non-negative integer")
    return value


def _reason(error):
    return error.strerror or str(error)


def _nearby(path, error):
    if not isinstance(error, (FileNotFoundError, NotADirectoryError)):
        return ""
    parent = os.path.dirname(path) or "."
    while not os.path.isdir(parent):
        parent = os.path.dirname(parent) or "."
    try:
        with os.scandir(parent) as entries:
            names = sorted(entry.name for entry in itertools.islice(entries, 25))
    except OSError:
        return ""
    names = ["".join(c if c.isprintable() else "?" for c in name) for name in names]
    shown = ", ".join(names[:24]) + (", and more" if len(names) > 24 else "")
    return f"; {parent} has: {shown}" if names else f"; {parent} is empty"


def _open_regular(path):
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NONBLOCK | os.O_NOCTTY | os.O_CLOEXEC)
    except OSError as error:
        raise BartError(f"cannot access {path}: {_reason(error)}{_nearby(path, error)}") from None
    info = os.fstat(fd)
    if not stat.S_ISREG(info.st_mode):
        os.close(fd)
        raise BartError(f"{path} is not a regular file")
    return os.fdopen(fd, "rb", buffering=_CHUNK), info


class _ReadText(Text):
    def __new__(cls, text, name, first, total, counted_all):
        value = super().__new__(cls, text)
        value._view = (name, first, total, counted_all)
        return value

    def __getnewargs__(self):
        return (str(self), *self._view)

    def _view_parts(self):
        name, first, total, counted_all = self._view
        if not self:
            status = "empty file" if counted_all and total == 0 else "no lines"
            yield _bart("read", f"{name}: {status}")
            return
        count = self.count("\n") + (not self.endswith("\n"))
        yield _bart("read", f"{name}:{first}-{first + count - 1}")
        start = 0
        index = first
        while start < len(self):
            end = self.find("\n", start)
            if end < 0:
                end = len(self)
            row = self[start:end].removesuffix("\r")
            yield f"\n{index:6d}\t{row}"
            start = end + 1
            index += 1

    def __repr__(self):
        return "".join(self._view_parts())

    def _display(self):
        for part in self._view_parts():
            sys.stdout.write(part)
        sys.stdout.write("\n")


def read(path, offset=1, limit=None):
    """Return complete requested UTF-8 text; bare views stream numbered lines."""
    if isinstance(path, Job):
        path = path._saved_path()
    name = getattr(path, "reference", None)
    path = _path(path)
    if name is None:
        name = path
    first = max(1, _count(offset, "offset"))
    limit = None if limit is None else _count(limit, "limit")
    last = None if limit is None else first + limit - 1
    data, scanned, total, started, counted_all = bytearray(), 0, 0, False, False
    try:
        file, info = _open_regular(path)
        with file:
            if limit != 0:
                if first == 1 and limit is None and info.st_size > MAX_FILE_BYTES:
                    raise BartError(f"{path} exceeds the {MAX_FILE_BYTES >> 20} MiB text limit; "
                                    "read a smaller range with offset and limit")
                while scanned < MAX_SCAN_BYTES:
                    chunked = last is None and total + 1 >= first
                    size = min(_CHUNK, MAX_SCAN_BYTES - scanned)
                    part = file.read(size) if chunked else file.readline(size)
                    if not part:
                        counted_all = True
                        break
                    scanned += len(part)
                    if first <= total + 1 and (last is None or total < last):
                        if len(data) + len(part) > MAX_FILE_BYTES:
                            raise BartError(f"requested text exceeds the {MAX_FILE_BYTES >> 20} MiB limit; "
                                            "read a smaller range with offset and limit")
                        data.extend(part)
                    total += part.count(b"\n")
                    started = not part.endswith(b"\n")
                    if last is not None and total >= last:
                        counted_all = not file.peek(1)
                        break
                else:
                    counted_all = not file.peek(1)
                if counted_all and started:
                    total += 1
                if not counted_all and (last is None or total < last):
                    raise BartError(f"the {MAX_SCAN_BYTES >> 20} MiB scan limit was reached before "
                                    f"the requested range of {path} was complete")
    except OSError as error:
        raise BartError(f"cannot read {path}: {_reason(error)}") from None
    if b"\0" in data:
        raise BartError(f"{path} contains NUL bytes; read expects UTF-8 text")
    try:
        text = data.decode("utf-8")
    except UnicodeDecodeError:
        raise BartError(f"{path} is not valid UTF-8 text") from None
    name = "".join(c if c.isprintable() else ascii(c)[1:-1] for c in name)
    if len(name) > 56:
        name = name[:19] + "…" + name[-36:]
    return _ReadText(text, name, first, total, counted_all)


def _identity(info):
    return (info.st_dev, info.st_ino, info.st_size, info.st_mtime_ns, info.st_ctime_ns,
            info.st_mode, info.st_nlink)


class _Target:

    def __init__(self, path, create):
        self.path = path
        self.data = None
        try:
            self.existing = os.stat(path)
        except FileNotFoundError:
            if os.path.islink(path):
                raise BartError(f"{path} is a symlink to a missing file") from None
            if not create:
                raise BartError(f"{path} does not exist") from None
            self.existing = None
            parent, name = os.path.split(path)
            if name in ("", ".", ".."):
                raise BartError(f"{path} is not a file path") from None
            _make_dirs(parent or ".", path)
        except OSError as error:
            raise BartError(f"cannot access {path}: {_reason(error)}") from None
        else:
            if not stat.S_ISREG(self.existing.st_mode):
                raise BartError(f"{path} is not a regular file")
            parent, name = os.path.split(os.path.realpath(path))
        self.name = name
        try:
            self.dir = os.open(parent or ".", os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC)
        except OSError as error:
            raise BartError(f"cannot access {path}: {_reason(error)}") from None

    def __enter__(self):
        self.fd = None
        try:
            if self.existing is not None:
                # Resolve the writable descriptor before reading the source. On
                # overlayfs, opening for write copies up a lower-layer file and
                # changes ctime even though its contents are unchanged.
                self.fd = os.open(self.name, os.O_RDWR | os.O_NONBLOCK | os.O_NOFOLLOW |
                                  os.O_NOCTTY | os.O_CLOEXEC, dir_fd=self.dir)
                info = os.fstat(self.fd)
                before, after = _identity(self.existing), _identity(info)
                if before[:4] + before[5:] != after[:4] + after[5:]:
                    raise OSError(0, "the file changed while opening it")
                self.existing = info
            return self
        except OSError as error:
            if self.fd is not None:
                os.close(self.fd)
            os.close(self.dir)
            raise BartError(f"cannot open {self.path} for writing: {_reason(error)}") from None

    def __exit__(self, *_):
        if self.fd is not None:
            os.close(self.fd)
        os.close(self.dir)

    def _unchanged(self, info):
        if _identity(info) != _identity(self.existing):
            raise OSError(0, "the file changed since it was resolved")

    def read(self):
        try:
            fd = os.open(self.name, os.O_RDONLY | os.O_NONBLOCK | os.O_NOFOLLOW | os.O_NOCTTY | os.O_CLOEXEC,
                         dir_fd=self.dir)
            with os.fdopen(fd, "rb") as file:
                info = os.fstat(fd)
                if not stat.S_ISREG(info.st_mode):
                    raise OSError(0, "not a regular file")
                self._unchanged(info)
                data = file.read(MAX_FILE_BYTES + 1)
                if len(data) > MAX_FILE_BYTES:
                    raise OSError(0, f"larger than the {MAX_FILE_BYTES >> 20} MiB edit limit")
                self._unchanged(os.fstat(fd))
                self.data = data
                return data
        except OSError as error:
            raise BartError(f"cannot read {self.path}: {_reason(error)}") from None

    def commit(self, data, change):
        flags = os.O_WRONLY | os.O_NONBLOCK | os.O_NOFOLLOW | os.O_NOCTTY | os.O_CLOEXEC
        if self.existing is None:
            flags |= os.O_CREAT | os.O_EXCL
        started = False
        _track_file(self.path)
        try:
            if self.existing is not None:
                self._unchanged(os.stat(self.name, dir_fd=self.dir, follow_symlinks=False))
                fd = os.dup(self.fd)
            else:
                fd = os.open(self.name, flags, 0o666, dir_fd=self.dir)
            with os.fdopen(fd, "wb") as file:
                if self.existing is not None:
                    self._unchanged(os.fstat(fd))
                    if self.data is not None and os.pread(fd, len(self.data) + 1, 0) != self.data:
                        raise OSError(0, "the file changed since it was resolved")
                started = True
                file.truncate()
                file.write(data)
        except FileExistsError:
            raise BartError(f"cannot write {self.path}: the file was created meanwhile") from None
        except OSError as error:
            partial = "; the file may now be incomplete, so inspect it before retrying" if started else ""
            raise BartError(f"cannot write {self.path}: {_reason(error)}{partial}") from None
        _changed[f"{change} {self.path}"] = None


def _make_dirs(parent, path):
    try:
        os.makedirs(parent, exist_ok=True)
    except OSError as error:
        raise BartError(f"cannot create the directories of {path}: {_reason(error)}") from None


def _uses_crlf(text):
    breaks = text.count("\n")
    return breaks > 0 and text.count("\r\n") == breaks


def _with_line_endings(text, crlf):
    return text.replace("\r\n", "\n").replace("\n", "\r\n") if crlf else text


def _with_final_newline(before, after, newline):
    wanted = before.endswith(b"\n") if before else bool(after)
    if wanted:
        return after + newline if after and not after.endswith("\n") else after
    while after.endswith("\n"):
        after = after[:-2] if after.endswith("\r\n") else after[:-1]
    return after


def _too_large(path):
    return BartError(f"the result for {path} would exceed {MAX_FILE_BYTES >> 20} MiB, the most Bart writes")


def _decode(data, path):
    try:
        return data.decode("utf-8")
    except UnicodeDecodeError:
        raise BartError(f"{path} is not UTF-8 text") from None


_file_originals = {}
_writable_files = weakref.WeakKeyDictionary()
_original_bytes = 0
_tracking_full = False
_diffs = True
_MAX_TRACKED_FILES = 256
_PROJECT = os.path.realpath(os.getcwd())
# Temporary files outside the project are scratch work, not changes to review.
_SCRATCH = tuple({os.path.realpath(root) for root in (os.environ.get("TMPDIR"), "/tmp", "/var/tmp") if root})


def _within(path, root):
    return path == root or path.startswith(root.rstrip(os.sep) + os.sep)


def _file_snapshot(path, limit):
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NONBLOCK | os.O_CLOEXEC)
        with io.open(fd, "rb") as file:
            info = os.fstat(file.fileno())
            if not stat.S_ISREG(info.st_mode):
                return None
            fingerprint = _identity(info)
            if info.st_size > limit:
                return fingerprint, None
            data = file.read(limit + 1)
            return fingerprint, data if len(data) <= limit else None
    except FileNotFoundError:
        return None, b""
    except (OSError, ValueError):
        return None


def _track_file(path):
    global _original_bytes, _tracking_full
    if not _in_cell or not _diffs or isinstance(path, int):
        return
    try:
        path = os.path.realpath(os.fsdecode(os.fspath(path)))
        if path in _file_originals:
            return
        if not _within(path, _PROJECT) and any(_within(path, root) for root in _SCRATCH):
            return
        if len(_file_originals) >= _MAX_TRACKED_FILES:
            _tracking_full = True
            return
        snapshot = _file_snapshot(path, MAX_DIFF_BYTES - _original_bytes)
        if snapshot is not None:
            _file_originals[path] = snapshot
            _original_bytes += len(snapshot[1]) if snapshot[1] is not None else 0
    except (OSError, TypeError, ValueError):
        pass


def _open_tracked(original):
    @functools.wraps(original)
    def opened(*args, **kwargs):
        file = args[0] if args else kwargs.get("file", kwargs.get("self"))
        mode = args[1] if len(args) > 1 else kwargs.get("mode", "r")
        writable = isinstance(mode, str) and any(flag in mode for flag in "wax+")
        tracked = writable and not isinstance(file, int) and kwargs.get("opener") is None
        if tracked:
            _track_file(file)
        result = original(*args, **kwargs)
        if tracked:
            try:
                if len(_writable_files) < _MAX_TRACKED_FILES:
                    _writable_files[result] = os.path.realpath(os.fsdecode(os.fspath(file)))
            except (TypeError, ValueError, OSError):
                pass
        return result
    return opened


def _paths_tracked(original, *names):
    @functools.wraps(original)
    def changed(*args, **kwargs):
        if all(kwargs.get(name) is None for name in ("dir_fd", "src_dir_fd", "dst_dir_fd")):
            for index, name in enumerate(names):
                _track_file(args[index] if index < len(args) else kwargs.get(name))
        return original(*args, **kwargs)
    return changed


def _finish_file_diffs():
    used = _original_bytes
    try:
        for path, (identity, before) in _file_originals.items():
            shown = os.path.relpath(path)
            if shown.split(os.sep)[0] == os.pardir:
                shown = path
            path_bytes = len(shown.encode())
            after = _file_snapshot(path, max(0, MAX_DIFF_BYTES - used - path_bytes))
            if after is None:
                continue
            current, data = after
            if before is not None and data is not None and before == data:
                continue
            if (before is None or data is None) and identity == current:
                continue
            summary = None
            if before is None or data is None:
                summary = "File changed; contents exceed the cell diff budget."
            elif b"\0" in before or b"\0" in data:
                summary = "Binary file changed; text diff not shown."
            else:
                try:
                    old, new = before.decode("utf-8"), data.decode("utf-8")
                except UnicodeDecodeError:
                    summary = "Binary file changed; text diff not shown."
            if summary is not None:
                _exchange({"diffsummary": {"path": shown, "summary": summary}})
            else:
                _exchange({"diff": {"path": shown, "before": old, "after": new}})
            used += path_bytes + (len(data) if data is not None else 0)
        if _tracking_full:
            _exchange({"diffsummary": {"path": "Python file changes", "summary": "Additional paths omitted: cell tracking limit reached."}})
    finally:
        _file_originals.clear()


builtins.open = _open_tracked(builtins.open)
io.open = _open_tracked(io.open)
pathlib.Path.open = _open_tracked(pathlib.Path.open)
for _name, _paths in (
    ("rename", ("src", "dst")),
    ("replace", ("src", "dst")),
    ("unlink", ("path",)),
    ("remove", ("path",)),
):
    _original = getattr(os, _name)
    _wrapped = _paths_tracked(_original, *_paths)
    for _support in (os.supports_dir_fd, os.supports_fd, os.supports_follow_symlinks):
        if _original in _support:
            _support.add(_wrapped)
    setattr(os, _name, _wrapped)
pathlib.Path.rename = _paths_tracked(pathlib.Path.rename, "self", "target")
pathlib.Path.replace = _paths_tracked(pathlib.Path.replace, "self", "target")
pathlib.Path.unlink = _paths_tracked(pathlib.Path.unlink, "self")


def write(path, content):
    """Creates or replaces a file, creating missing directories."""
    path = _path(path)
    if not isinstance(content, str):
        raise BartError("content must be a string")
    if len(content) > MAX_FILE_BYTES:
        raise _too_large(path)
    with _Target(path, create=True) as target:
        before = target.read() if target.existing is not None else None
        try:
            crlf = before is not None and _uses_crlf(before.decode("utf-8"))
        except UnicodeDecodeError:
            crlf = False
        data = _with_final_newline(before, _with_line_endings(content, crlf), "\r\n" if crlf else "\n").encode()
        if len(data) > MAX_FILE_BYTES:
            raise _too_large(path)
        target.commit(data, "wrote")


def _edit_entry(entry):
    if isinstance(entry, dict):
        if not {"old", "new"} <= entry.keys() or entry.keys() - {"old", "new", "replace_all", "within"}:
            raise BartError("Use edit(path, edits=[{\"old\": old, \"new\": new}]); entries may also have replace_all and within. No changes from this edit call")
        return entry["old"], entry["new"], entry.get("replace_all", False), entry.get("within")
    if isinstance(entry, (list, tuple)) and len(entry) in (2, 3):
        return entry[0], entry[1], False, entry[2] if len(entry) == 3 else None
    raise BartError("Use edit(path, edits=[(old, new), ...]) with (old, new), (old, new, within) or dictionaries with old and new. No changes from this edit call")


def _valid(old, new, replace_all, within):
    if not isinstance(old, str) or not isinstance(new, str):
        raise BartError("Call edit with strings for old and new. No changes from this edit call")
    if not old:
        raise BartError("Call edit with non-empty old text. No changes from this edit call")
    if not isinstance(replace_all, bool):
        raise BartError("Call edit with replace_all=True or False. No changes from this edit call")
    if within is not None:
        if (not isinstance(within, (list, tuple)) or len(within) != 2
                or any(isinstance(n, bool) or not isinstance(n, int) for n in within)
                or not 1 <= within[0] <= within[1]):
            raise BartError("Call edit with within=(start, end), a 1-based inclusive range with start <= end. No changes from this edit call")
        within = (within[0], within[1])
    return [old, new, replace_all, within]


def _matches(text, old, every, start=0, end=None):
    end = len(text) if end is None else end
    step = len(old) if every else 1
    at = text.find(old, start, end)
    while at != -1:
        yield at
        at = text.find(old, at + step, end)


def _line_range(text, first, last):
    begin = 0 if first == 1 else None
    line, at = 1, text.find("\n")
    while at != -1:
        if line == last:
            return (len(text) if begin is None else begin), at + 1
        line += 1
        if line == first:
            begin = at + 1
        at = text.find("\n", at + 1)
    return (len(text) if begin is None else begin), len(text)


def _place(items, index, path):
    entry = f"edit #{index + 1}: " if len(items) > 1 else ""
    within = items[index][3]
    where = f"lines {within[0]}..={within[1]} of {path}" if within else path
    return f"{entry}old text in {where}"


def edit(path, old=None, new=None, replace_all=None, within=None, *, edits=None):
    """Replaces exact text in one file: `old` with `new`, or each `(old, new)`, `(old, new, within)`
    or `{"old", "new", "replace_all", "within"}` in `edits`, all matched against the original file.
    If any edit does not match, the file is left unchanged. The file is rewritten in place, so a
    failed write can leave it partly written."""
    path = _path(path)
    if edits is None:
        if old is None or new is None:
            raise BartError("Use edit(path, old, new) or edit(path, edits=[(old, new), ...]). No changes from this edit call")
        items = [(old, new, bool(replace_all) if replace_all is not None else False, within)]
        if replace_all is not None and not isinstance(replace_all, bool):
            raise BartError("Call edit with replace_all=True or False. No changes from this edit call")
    else:
        if old is not None or new is not None or replace_all is not None or within is not None:
            raise BartError("Call edit(path, edits=[...]) with old, new, replace_all and within inside each entry, not at the top level. No changes from this edit call")
        if not isinstance(edits, (list, tuple)):
            raise BartError("Call edit(path, edits=[(old, new), ...]). No changes from this edit call")
        items = [_edit_entry(entry) for entry in edits]
    if not 1 <= len(items) <= MAX_EDITS:
        raise BartError(f"Call edit with 1–{MAX_EDITS} entries. No changes from this edit call")
    items = [_valid(*item) for item in items]
    with _Target(path, create=False) as target:
        text = _decode(target.read(), path)
        crlf = _uses_crlf(text)
        if sum(len(item[0]) + len(item[1]) for item in items) * (2 if crlf else 1) > MAX_FILE_BYTES:
            raise _too_large(path)
        for index, item in enumerate(items):
            item[0], item[1] = _with_line_endings(item[0], crlf), _with_line_endings(item[1], crlf)
            if item[0] == item[1]:
                raise BartError(f"{f'edit #{index + 1}: ' if len(items) > 1 else ''}old and new are identical after line-ending conversion; "
                                "omit this unchanged pair. No changes from this edit call")
        regions, size = [], len(text)
        for index, (old, new, every, within) in enumerate(items):
            begin, end = _line_range(text, *within) if within else (0, len(text))
            found = list(itertools.islice(_matches(text, old, every, begin, end), MAX_REPLACEMENTS + 1))
            if not found:
                raise BartError(f"{_place(items, index, path)} not found; inspect the current file and copy exact text "
                                "without line-number prefixes (for literal escapes, inspect repr of Python file I/O). "
                                "Batch entries match the original file, not earlier replacements. No changes from this edit call")
            if not every and len(found) > 1:
                lines = ", ".join(str(text.count("\n", 0, at) + 1) for at in found[:MAX_MATCH_LINES])
                count = sum(1 for _ in _matches(text, old, False, begin, end))
                more = ", …" if count > MAX_MATCH_LINES else ""
                raise BartError(f"{_place(items, index, path)} occurs {count} times, at lines {lines}{more}; "
                                "include unique context or use within=(start, end) for one location; "
                                "use replace_all=True only if every occurrence should change. No changes from this edit call")
            regions.extend((at, at + len(old), index) for at in found)
            if len(regions) > MAX_REPLACEMENTS:
                raise BartError(f"Call edit with at most {MAX_REPLACEMENTS} replacements. No changes from this edit call")
            size += len(found) * (len(new) - len(old))
        if size > MAX_FILE_BYTES:
            raise _too_large(path)
        regions.sort()
        for (_, end, a), (begin, _, b) in zip(regions, regions[1:]):
            if end > begin:
                raise BartError(f"edits #{min(a, b) + 1} and #{max(a, b) + 1} overlap in {path}; combine them into one replacement in edit(path, edits=[...]). No changes from this edit call")
        parts, previous = [], 0
        for begin, end, index in regions:
            parts += (text[previous:begin], items[index][1])
            previous = end
        parts.append(text[previous:])
        data = _with_final_newline(text.encode(), "".join(parts), "\r\n" if crlf else "\n").encode()
        if len(data) > MAX_FILE_BYTES:
            raise _too_large(path)
        target.commit(data, "edited")


class _ImageBuffer(io.BytesIO):
    def write(self, data):
        if self.tell() + len(data) > MAX_IMAGE_BYTES:
            raise BartError(f"the image is larger than {MAX_IMAGE_BYTES >> 20} MiB")
        return super().write(data)


def _show_image(image):
    with _ImageBuffer() as data:
        image.save(data, format="PNG")
        _call("image", label="", data=base64.b64encode(data.getvalue()).decode("ascii"))


def _is_image(value):
    return isinstance(value, getattr(sys.modules.get("PIL.Image"), "Image", ()))


def display(*images):
    """Shows you images after this cell's result: image file paths (PNG, JPEG, GIF or WebP) or
    PIL images."""
    for image in images:
        if _is_image(image):
            _show_image(image)
            continue
        if not isinstance(image, (str, os.PathLike)):
            raise BartError("display takes image file paths or PIL images")
        path = _path(image)
        file, _ = _open_regular(path)
        with file:
            data = file.read(MAX_IMAGE_BYTES + 1)
        saved = pathlib.Path(path)
        stem = saved.name.split(".", 1)[0]
        label = (_bart("image", f"image[{stem[6:]}]") if saved.parent == _SAVED and stem.startswith("image-")
                 and stem[6:].isdigit() else _bart("image", path))
        _call("image", label=label, data=base64.b64encode(data).decode("ascii"))


class Mcp:
    """The MCP servers' tools. `mcp(server, tool, **arguments)` calls one and returns its structured
    result, or its text."""

    def __call__(self, server, tool, /, **arguments):
        result = _call("mcp_call", server=server, tool=tool, arguments=arguments)
        _changed[f"called MCP tool {tool} on {server}"] = None
        for note in result["notes"]:
            _diagnostic(note)
        if result["error"] is not None:
            raise BartError(result["error"])
        value = result["value"]
        return Text(value) if isinstance(value, str) else value

    def servers(self):
        """Each server's current state as a dict."""
        return _call("mcp_servers")

    def tools(self, server=None, query=None):
        """Tool names with their first description line: one server's, or all servers' matching
        `query` (case-insensitive)."""
        return Text(_call("mcp_tools", server=server, query=query))

    def describe(self, server, tool):
        """A tool's full description and input schema."""
        return Text(_call("mcp_describe", server=server, tool=tool))


_inventory_json_dumps = json.dumps


def _safe_namespace_inventory():
    try:
        return _namespace_inventory()
    except BaseException:
        return {"unavailable": "namespace inspection failed"}


def _namespace_inventory():
    # Exact-type checks avoid repr, properties and user serialization hooks.
    entries = []
    used = 0
    omitted = 0
    for name, value in list(namespace.items()):
        if type(name) is not str or name.startswith("__") or name in bart_builtins:
            continue
        if len(name) > 200:
            omitted += 1
            continue
        kind = type(value)
        entry = {"name": name, "kind": "object"}
        if any(kind is item for item in (type(None), bool, int, float, str)):
            entry["kind"] = kind.__name__
            if kind is str:
                entry["size"] = len(value)
        elif any(kind is item for item in (list, tuple, dict, set, frozenset, bytes)):
            entry.update(kind=kind.__name__, size=len(value))
        elif kind is types.FunctionType:
            code = value.__code__
            entry.update(kind="function", parameters=list(code.co_varnames[:min(32, code.co_argcount + code.co_kwonlyargcount)]),
                         source=code.co_filename if code.co_filename != "<cell>" else None, line=code.co_firstlineno,
                         positional_only=code.co_posonlyargcount, keyword_only=code.co_kwonlyargcount,
                         varargs=bool(code.co_flags & 4), kwargs=bool(code.co_flags & 8))
        elif kind is types.ModuleType:
            entry["kind"] = "module"
        elif kind is pathlib.PosixPath or kind is pathlib.WindowsPath:
            entry["kind"] = "path"
        size = len(_inventory_json_dumps(entry, ensure_ascii=True))
        if len(entries) >= 128 or used + size > 12000:
            omitted += 1
            continue
        used += size
        entries.append(entry)
    return {"bindings": entries, "omitted": omitted, "scope": "all eligible globals"}


def _observe_file(function):
    @functools.wraps(function)
    def observed(path, *args, **kwargs):
        return _activity(f"{function.__name__}({_path(path)!r})",
                         lambda: function(path, *args, **kwargs))
    return observed


read, write, edit = map(_observe_file, (read, write, edit))

bart_builtins = dict(whoami=whoami, run=run, RunError=RunError, bg_run=bg_run, job=job, read=read, write=write, edit=edit, display=display, mcp=Mcp(),
                     Text=Text, Result=Result, SavedStream=SavedStream, BartError=BartError, Job=Job, Cell=Cell, Edit=dict)
# Trailing double underscores prevent class-name mangling of the injected hook.
_DISPLAY_HOOK = "__bart_display__"


def _display_value(value):
    if isinstance(value, (_ReadText, Result)) and not isinstance(value, Job):
        value._display()
    elif isinstance(value, Text):
        print(repr(value))


class _ExpressionDisplay(ast.NodeTransformer):
    def visit_Expr(self, node):
        if isinstance(node.value, ast.Constant):
            return node
        node = self.generic_visit(node)
        node.value = ast.copy_location(ast.Call(
            func=ast.Name(id=_DISPLAY_HOOK, ctx=ast.Load()),
            args=[node.value], keywords=[]), node.value)
        return node


helpers = dict(vars(builtins), **bart_builtins)
# Common words: a cell's own binding of one of these wins instead of being removed.
helpers.update(cell=cell, image=image, paste=paste, history=history)
helpers[_DISPLAY_HOOK] = _display_value
namespace = {"__name__": "__main__", "__builtins__": helpers, "collections": collections, "json": json,
             "os": os, "pathlib": pathlib, "re": re, "sys": sys, "time": time}


def restore():
    """Removes globals that shadow a helper, and says which."""
    names = bart_builtins.keys() | {_DISPLAY_HOOK} | ({"web_search"} if "web_search" in helpers else set())
    if rebound := sorted(names & namespace.keys()):
        for name in rebound:
            del namespace[name]
        _diagnostic("\n" + _bart("cell", f"cell[{_activity_cell}] helper bindings removed; {', '.join(rebound)} are built-ins"))


class _CellStackSummary(traceback.StackSummary):
    def __init__(self, frames, filename, source):
        super().__init__(frames)
        self.filename = filename
        self.source = source

    def format_frame_summary(self, frame, **kwargs):
        if (frame.filename != self.filename or any(
                getattr(frame, field, None) is None
                for field in ("colno", "end_colno", "end_lineno"))):
            return super().format_frame_summary(frame, **kwargs)
        position = types.SimpleNamespace(
            lineno=frame.lineno, end_lineno=frame.end_lineno,
            col_offset=frame.colno, end_col_offset=frame.end_colno)
        try:
            segment = ast.get_source_segment(self.source, position, padded=True)
        except (IndexError, ValueError):
            segment = None
        if segment is None:
            return super().format_frame_summary(frame, **kwargs)
        lines = textwrap.dedent(segment).lstrip().split("\n")
        if len(lines) > 6:
            lines = [*lines[:3], "...", *lines[-3:]]
        header = super().format_frame_summary(traceback.FrameSummary(
            frame.filename, frame.lineno, frame.name, lookup_line=False, line=""), **kwargs)
        return header + textwrap.indent("\n".join(lines), "    ") + "\n"


def execute_cell(code, diffs, cell_number):
    filename = f"cell[{cell_number}].code"
    global _in_cell, _interrupted, _original_bytes, _tracking_full, _diffs, _activity_cell
    try:
        # Discover packages installed since the last cell, including newly
        # created user-site directories, without executing .pth startup files.
        importlib.invalidate_caches()
        _interrupted = False
        _changed.clear()
        _original_bytes = 0
        _tracking_full = False
        _file_originals.clear()
        _in_cell = True
        _activity_cell = cell_number
        _diffs = diffs
        if _diffs:
            for file, path in list(_writable_files.items()):
                if file.closed:
                    del _writable_files[file]
                else:
                    _track_file(path)
        with warnings.catch_warnings():
            warnings.filterwarnings("ignore", ".*invalid escape sequence", SyntaxWarning)
            tree = ast.parse(code, filename, "exec")
        compile(tree, filename, "exec")
        last = tree.body.pop() if tree.body and isinstance(tree.body[-1], ast.Expr) else None
        tree = ast.fix_missing_locations(_ExpressionDisplay().visit(tree))
        exec(compile(tree, filename, "exec"), namespace)
        if last is not None:
            value = eval(compile(ast.Expression(last.value), filename, "eval"), namespace)
            if _is_image(value):
                _show_image(value)
            elif isinstance(value, (_ReadText, Result)) and not isinstance(value, Job):
                value._display()
            elif value is not None and not isinstance(value, Job):
                print(repr(value))
        if _interrupted:
            raise KeyboardInterrupt
        return False
    except BaseException as failure:
        # Virtual cell filenames have no filesystem source. Cache only while
        # extracting the report (which eagerly looks up lines), not across cells.
        # Python source uses universal newlines, not str.splitlines(): form feed
        # and Unicode separators inside comments must not shift line numbers.
        source = code.replace("\r\n", "\n").replace("\r", "\n")
        previous = linecache.cache.get(filename)
        linecache.cache[filename] = (
            len(code), None, [line + "\n" for line in source.split("\n")], filename)
        try:
            report = traceback.TracebackException.from_exception(failure)
        finally:
            if previous is None:
                linecache.cache.pop(filename, None)
            else:
                linecache.cache[filename] = previous
        pending = [report]
        attributed = False
        while pending:
            current = pending.pop()
            current.stack = _CellStackSummary(
                [f for f in current.stack if f.filename != "<bart-harness>"], filename, source)
            attributed |= (any(frame.filename == filename for frame in current.stack)
                           or getattr(current, "filename", None) == filename)
            pending.extend(e for e in (current.__cause__, current.__context__) if e is not None)
            pending.extend(getattr(current, "exceptions", None) or [])
        text = "".join(report.format())
        if (sys.version_info < (3, 12) and isinstance(failure, NameError)
                and failure.name in sys.stdlib_module_names):
            text = f"{text.rstrip()}. Did you forget to import '{failure.name}'?\n"
        if not attributed:
            _diagnostic(_bart("cell", f"cell[{cell_number}] failed"))
        _diagnostic(text)
        return True
    finally:
        _in_cell = False
        try:
            if _diffs:
                _finish_file_diffs()
        except (OSError, ValueError, BartError):
            pass


while line := _read_line():
    try:
        request = json.loads(line)["run"]
        if request["web_search"]:
            helpers["web_search"] = web_search
        else:
            helpers.pop("web_search", None)
        failed = execute_cell(request["code"], request["diffs"], request["cell"])
    except KeyboardInterrupt:
        _diagnostic(_bart("cell", f"cell[{request['cell']}] interrupted"))
        failed = True
    restore()
    _diagnostic()
    with _channel:
        os.write(2, b"\n" + MARKER + b"\n")
        replies.write(_inventory_json_dumps({"done": {"failed": failed, "changed": list(_changed), "inventory": _safe_namespace_inventory()}}) + "\n")
        replies.flush()
