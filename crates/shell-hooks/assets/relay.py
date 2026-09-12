"""Private hook queue and HTTP-over-UDS sender. Python 3 standard library only."""
import http.client
import json
import os
from pathlib import Path
import re
import select
import socket
import stat
import subprocess
import sys
import time
import uuid

ROOT = Path(__file__).resolve().parent
QUEUE = ROOT / "queue"
ERRORS = ROOT / "errors"
MAX_BYTES = 65536
MAX_PENDING = 256
MAX_ERRORS = 32
# A transient delivery failure retries on later drains; after MAX_ATTEMPTS
# failures the event is quarantined. The count lives in the on-disk name.
MAX_ATTEMPTS = 5
NAME = re.compile(r"[a-f0-9]{32}\.json\Z")
RETRY = re.compile(r"[a-f0-9]{32}\.retry[1-%d]\Z" % (MAX_ATTEMPTS - 1))
EVENT = re.compile(r"[A-Za-z][A-Za-z0-9_.-]{0,127}\Z")


def private_directory(path):
    info = path.lstat()
    if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
        raise ValueError("queue directory is not private")


def read_regular(path):
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(fd, "rb") as stream:
        info = os.fstat(stream.fileno())
        if not stat.S_ISREG(info.st_mode) or info.st_size > MAX_BYTES:
            raise ValueError("invalid queue file")
        data = stream.read(MAX_BYTES + 1)
    if len(data) > MAX_BYTES:
        raise ValueError("hook body too large")
    return data


def context():
    return json.loads(read_regular(ROOT / "context.json"))


def checked_name(name):
    # A `.retry<N>` suffix keeps a failed event out of fresh admission without
    # hiding it from later drains, and cannot collide with a new UUID name.
    if not (NAME.fullmatch(name) or RETRY.fullmatch(name)):
        raise ValueError("invalid queue name")
    return QUEUE / name


def envelope(name):
    raw = read_regular(checked_name(name))
    data = json.loads(raw)
    if not isinstance(data, dict) or data.get("binding") != context()["binding"]:
        raise ValueError("queue binding mismatch")
    if not isinstance(data.get("event_name"), str) or not EVENT.fullmatch(data["event_name"]):
        raise ValueError("invalid event name")
    if not isinstance(data.get("event_id"), str) or not 0 < len(data["event_id"]) <= 256:
        raise ValueError("invalid event ID")
    if not isinstance(data.get("payload"), dict):
        raise ValueError("invalid event payload")
    return raw


def bounded_stdin():
    if sys.stdin.isatty():
        return b"{}"
    deadline = time.monotonic() + 2
    chunks = bytearray()
    while True:
        remaining = deadline - time.monotonic()
        if remaining <= 0 or not select.select([sys.stdin], [], [], remaining)[0]:
            raise ValueError("hook stdin timeout")
        block = os.read(sys.stdin.fileno(), min(8192, MAX_BYTES + 1 - len(chunks)))
        if not block:
            break
        chunks.extend(block)
        if len(chunks) > MAX_BYTES:
            raise ValueError("hook payload too large")
    return chunks or b"{}"


def quarantine(path, reason):
    # Never read or follow a poison file, including symlinks, FIFOs and oversized data.
    # Store a bounded diagnostic, not prompts, tokens or arbitrary file contents.
    old = sorted(ERRORS.glob("*.json"))
    for item in old[:max(0, len(old) - MAX_ERRORS + 1)]:
        if not item.is_dir():
            item.unlink(missing_ok=True)
    record = {"file": path.name[:128], "reason": reason[:128]}
    target = ERRORS / (uuid.uuid4().hex + ".json")
    fd = os.open(target, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(fd, "w") as out:
        json.dump(record, out)
    # An unexpected directory is never recursively removed.
    try:
        path.unlink(missing_ok=True)
    except IsADirectoryError:
        pass


def enqueue(args):
    if not 1 <= len(args) <= 2 or not EVENT.fullmatch(args[0]):
        raise ValueError("expected a safe event name and optional JSON argument")
    raw = args[1].encode() if len(args) == 2 else bounded_stdin()
    if len(raw) > MAX_BYTES:
        raise ValueError("hook payload too large")
    payload = json.loads(raw)
    if not isinstance(payload, dict):
        raise ValueError("hook payload must be an object")
    event_id = payload.get("noches_event_id")
    if not isinstance(event_id, str) or not 0 < len(event_id) <= 256:
        event_id = uuid.uuid4().hex
    data = json.dumps({"binding": context()["binding"], "event_id": event_id,
                       "event_name": args[0], "payload": payload}).encode()
    if len(data) > MAX_BYTES:
        raise ValueError("hook envelope too large")
    # Serialize admission to keep the queue bounded even during provider fan-out.
    lock = ROOT / "enqueue.lock"
    deadline = time.monotonic() + 2
    while True:
        try:
            lock.mkdir(mode=0o700)
            break
        except FileExistsError:
            if time.monotonic() >= deadline:
                raise ValueError("queue admission busy")
            time.sleep(0.01)
    try:
        if sum(1 for _ in QUEUE.iterdir()) >= MAX_PENDING:
            raise ValueError("hook queue full")
        name = uuid.uuid4().hex
        temporary = QUEUE / (name + ".tmp")
        fd = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        try:
            with os.fdopen(fd, "wb") as out:
                out.write(data)
                out.flush()
                os.fsync(out.fileno())
            temporary.rename(QUEUE / (name + ".json"))
        finally:
            temporary.unlink(missing_ok=True)
    finally:
        lock.rmdir()


def batch():
    count = 0
    # A failed event stays queued under its `.retry<N>` name until a later
    # drain redelivers it. Poison names are quarantined without consuming the
    # per-drain delivery budget.
    for path in sorted(QUEUE.iterdir(), key=lambda p: (p.lstat().st_mtime_ns, p.name)):
        if path.suffix == ".tmp":
            continue
        if stat.S_ISDIR(path.lstat().st_mode):
            # Do not let an unexpected directory consume every future drain budget.
            continue
        if not (NAME.fullmatch(path.name) or RETRY.fullmatch(path.name)):
            quarantine(path, "invalid queue filename")
            continue
        if count == 10:
            break
        count += 1
        print(path.name)


class UnixHTTP(http.client.HTTPConnection):
    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(2)
        self.sock.connect(context()["socket"])


def send(raw):
    argv = context().get("notifier_argv")
    if argv:
        subprocess.run(argv, input=raw, check=True, timeout=2,
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        return
    conn = UnixHTTP("localhost", timeout=2)
    try:
        conn.request("POST", "/hooks", raw, {
            "Authorization": "Bearer " + context()["token"],
            "Content-Type": "application/json",
        })
        reply = conn.getresponse()
        if reply.status not in (200, 202, 204):
            raise ValueError("hook receiver rejected event")
        # No response body is needed, and an unbounded response is never read.
    finally:
        conn.close()


def main():
    os.umask(0o077)
    private_directory(ROOT)
    private_directory(QUEUE)
    private_directory(ERRORS)
    command, *args = sys.argv[1:]
    if command == "enqueue":
        enqueue(args)
    elif command == "batch":
        batch()
    elif command == "validate":
        envelope(args[0])
    elif command == "ack":
        checked_name(args[0]).unlink(missing_ok=True)
    elif command == "fail":
        fail(args[0])
    elif command == "deliver":
        send(envelope(args[0]))
    else:
        raise ValueError("unknown relay operation")


def fail(name):
    path = checked_name(name)
    match = RETRY.fullmatch(name)
    # A fresh name just failed its first delivery; `.retry<N>` its (N+1)-th.
    attempts = int(name.rsplit(".", 1)[1][len("retry"):]) + 1 if match else 1
    if attempts >= MAX_ATTEMPTS:
        quarantine(path, "delivery failed after %d attempts" % MAX_ATTEMPTS)
        return
    # The renamed event is invisible to admission and picked up by a later
    # drain, so one transient failure never becomes a permanent quarantine.
    path.rename(QUEUE / f"{name[:32]}.retry{attempts}")


if __name__ == "__main__":
    try:
        main()
    except (ValueError, OSError, IndexError, http.client.HTTPException, subprocess.SubprocessError):
        # Payloads and authentication details must not leak into provider hook output.
        print("noches hook relay failed", file=sys.stderr)
        sys.exit(1)
