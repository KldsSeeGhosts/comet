"""Codex notify adapter. No private history scraping or synthetic start events."""
import hashlib
import json
import os
import subprocess
import sys


def main():
    if len(sys.argv) != 2 or len(sys.argv[1].encode()) > 60000:
        return 1
    event = json.loads(sys.argv[1])
    if not isinstance(event, dict):
        return 1
    if event.get("type") != "agent-turn-complete":
        return 0
    thread, turn = event.get("thread-id"), event.get("turn-id")
    if not isinstance(thread, str) or not isinstance(turn, str) or not thread or not turn:
        return 1
    # The native turn ID provides a stable retry ID without hashing prompt contents.
    event["noches_event_id"] = hashlib.sha256(json.dumps([thread, turn, event["type"]]).encode()).hexdigest()
    result = subprocess.run(["/bin/sh", os.environ["NOCHES_NOTIFY"], event["type"], json.dumps(event)],
                            stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                            stderr=subprocess.DEVNULL, timeout=30, check=False)
    return result.returncode


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (ValueError, OSError, KeyError, subprocess.TimeoutExpired):
        sys.exit(1)
