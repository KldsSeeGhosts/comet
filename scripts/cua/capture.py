#!/usr/bin/env python3
"""Read-only, bounded capture for Noches/Hyprland input failures.

Does not inject input, read keystrokes, take screenshots, inspect environment
variables, stop processes, unlink sockets, change sysctls, or reload Hyprland.
The output stays on this machine in a newly created mode-0700 directory.
"""
from __future__ import annotations

import argparse
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import time
from typing import Any

MAX_OUTPUT = 2 * 1024 * 1024
AUTHORITY_FIELDS = ("reserved", "lease_active", "held_button", "held_keys", "drag_active")


def utc_now() -> str:
    return datetime.now(timezone.utc).isoformat()


def command(argv: list[str], timeout: float = 3.0) -> dict[str, Any]:
    started = time.monotonic()
    result: dict[str, Any] = {"argv": argv, "at": utc_now()}
    try:
        completed = subprocess.run(argv, capture_output=True, timeout=timeout, check=False)
        stdout = completed.stdout[:MAX_OUTPUT].decode("utf-8", "replace")
        stderr = completed.stderr[:MAX_OUTPUT].decode("utf-8", "replace")
        result.update(returncode=completed.returncode, stdout=stdout, stderr=stderr,
                      truncated=len(completed.stdout) > MAX_OUTPUT or len(completed.stderr) > MAX_OUTPUT)
        if completed.returncode == 0 and not result["truncated"]:
            try:
                result["json"] = json.loads(stdout)
            except (ValueError, TypeError):
                pass
    except (OSError, subprocess.TimeoutExpired) as error:
        result["error"] = str(error)
    result["elapsed_seconds"] = round(time.monotonic() - started, 4)
    return result


def summarize_status(value: Any) -> dict[str, Any]:
    """Classify observations, never infer driver ownership or a freeze cause."""
    if not isinstance(value, dict) or not isinstance(value.get("input"), dict):
        return {"state": "unavailable", "reason": "No parsed production input status"}
    block = value["input"]
    lanes = block.get("lanes")
    if not isinstance(lanes, list) or not lanes:
        return {"state": "incomplete", "reason": "Missing lane observations"}
    summaries = []
    for lane in lanes:
        if not isinstance(lane, dict):
            return {"state": "incomplete", "reason": "Invalid lane object"}
        if any(field not in lane for field in AUTHORITY_FIELDS):
            return {"state": "incomplete", "reason": "Missing authority fields"}
        if any(type(lane[field]) is not bool for field in ("reserved", "lease_active", "drag_active")):
            return {"state": "incomplete", "reason": "Invalid boolean authority field"}
        if any(type(lane[field]) is not int or lane[field] < 0 for field in ("held_button", "held_keys")):
            return {"state": "incomplete", "reason": "Invalid held-input count"}
        active = lane["lease_active"] or lane["drag_active"] or lane["held_button"] != 0 or lane["held_keys"] != 0
        summaries.append({
            "lane": lane.get("lane"),
            "active_authority_or_held_input": active,
            **{field: lane[field] for field in AUTHORITY_FIELDS},
            "pointer_focus": lane.get("pointer_focus"),
            "keyboard_focus": lane.get("keyboard_focus"),
            "dispatches": lane.get("dispatches"),
        })
    state = "active_authority_or_held_input" if any(lane["active_authority_or_held_input"] for lane in summaries) else (
        "reserved_without_active_authority" if any(lane["reserved"] for lane in summaries) else "no_active_authority_observed")
    return {"state": state, "transport_ready": block.get("transport_ready"), "lanes": summaries,
            "note": "A live driver may legitimately own a lane. Passive pointer focus and seat resource counts alone do not prove a leak."}


def process_inventory(proc_root: Path = Path("/proc")) -> list[dict[str, Any]]:
    """Record executable/parent identity, but no arbitrary command lines or env."""
    found = []
    allowed = {"cua-driver", "zeron", "zeron-dev", "quickshell", "qs", "Hyprland"}
    for entry in proc_root.iterdir():
        if not entry.name.isdecimal():
            continue
        try:
            comm = (entry / "comm").read_text().strip()
            executable = os.readlink(entry / "exe")
            if comm not in allowed and Path(executable.removesuffix(" (deleted)")).name not in allowed:
                continue
            status = (entry / "status").read_text()
            parent_match = re.search(r"^PPid:\s+(\d+)", status, re.MULTILINE)
            parent = int(parent_match.group(1)) if parent_match else None
            try:
                parent_name = (proc_root / str(parent) / "comm").read_text().strip()
            except OSError:
                parent_name = None
            item = {"pid": int(entry.name), "ppid": parent, "parent_comm": parent_name,
                    "comm": comm, "executable": executable}
            if comm == "cua-driver" or Path(executable).name == "cua-driver":
                words = (entry / "cmdline").read_bytes().split(b"\0")
                item["driver_mode"] = next((word.decode("ascii") for word in words[1:] if word in (b"serve", b"mcp")), None)
            found.append(item)
        except (OSError, ValueError):
            continue  # Process exit and permission races are expected.
    return sorted(found, key=lambda item: item["pid"])


def seat_sections(text: str) -> list[dict[str, Any]]:
    """Extract wayland-info observations, not the seats selected by other apps."""
    seats: list[dict[str, Any]] = []
    current = None
    for line in text.splitlines():
        if line.lstrip().startswith("interface:"):
            current = None
            match = re.search(r"interface:\s*'wl_seat'.*?version:\s*(\d+).*?name:\s*(\d+)", line)
            if match:
                current = {"version": int(match.group(1)), "registry_id": int(match.group(2))}
                seats.append(current)
        elif current is not None:
            stripped = line.strip()
            if stripped.startswith("name:"):
                current["seat_name"] = stripped.split(":", 1)[1].strip()
            elif stripped.startswith("capabilities:"):
                current["capabilities"] = stripped.split(":", 1)[1].strip()
    return seats


def read_optional(path: Path) -> str | None:
    try:
        return path.read_text().strip()
    except OSError:
        return None


def metadata() -> dict[str, Any]:
    home = Path.home()
    revisions = {}
    for name in ("comet", "cua"):
        repo = home / "AiStack" / name
        revisions[name] = {
            "head": command(["git", "-C", str(repo), "rev-parse", "HEAD"]),
            "worktree": command(["git", "--no-optional-locks", "-C", str(repo), "status", "--short"]),
        }
    return {
        "at": utc_now(),
        "monitors": command(["hyprctl", "-j", "monitors"]),
        "devices": command(["hyprctl", "-j", "devices"]),
        "layers": command(["hyprctl", "-j", "layers"]),
        "version": command(["hyprctl", "-j", "version"]),
        "service": command(["systemctl", "--user", "show", "zeron-dev.service", "--property=MainPID,ActiveState,ExecMainStartTimestamp"]),
        "ptrace_scope_unmodified": read_optional(Path("/proc/sys/kernel/yama/ptrace_scope")),
        "processes": process_inventory(),
        "repositories": revisions,
    }


def private_directory(root: Path) -> Path:
    root.mkdir(mode=0o700, parents=True, exist_ok=True)
    return Path(tempfile.mkdtemp(prefix=datetime.now().strftime("capture-%Y%m%d-%H%M%S-"), dir=root))


def write_json(path: Path, value: Any) -> None:
    with path.open("x", encoding="utf-8") as stream:
        os.chmod(path, 0o600)
        json.dump(value, stream, indent=2)
        stream.write("\n")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--watch", type=float, default=0, metavar="SECONDS", help="Bounded sampling period, 0 for one snapshot, maximum 600")
    parser.add_argument("--interval", type=float, default=0.5, help="Delay between status requests, minimum 0.1 seconds")
    parser.add_argument("--out-root", type=Path, default=Path.home() / ".local/state/noches-cua-debug")
    parser.add_argument("--wayland-info", action="store_true", help="Also run the optional wayland-info seat observer once; does not prove GPUI's own seat binding")
    args = parser.parse_args()
    if not 0 <= args.watch <= 600 or not 0.1 <= args.interval <= 60:
        parser.error("watch must be between 0 and 600; interval between 0.1 and 60")
    os.umask(0o077)
    output = private_directory(args.out_root.expanduser())
    print(f"Capture directory: {output}", flush=True)
    print("Read-only capture. No input injection, restarts, socket deletion, or sysctl changes.", flush=True)
    write_json(output / "before.json", metadata())
    if args.wayland_info:
        observed = command(["wayland-info"], timeout=5.0)
        observed["seats"] = seat_sections(observed.get("stdout", ""))
        observed["scope"] = "This observer's enumeration, not Noches or Quickshell bindings"
        write_json(output / "wayland-info.json", observed)
    deadline = time.monotonic() + args.watch
    print("Sampling plugin status now. Reproduce the Noches turn while sampling.", flush=True)
    seen_states: dict[str, int] = {}
    previous = None
    samples = 0
    interrupted = False
    with (output / "samples.jsonl").open("x", encoding="utf-8") as stream:
        os.chmod(output / "samples.jsonl", 0o600)
        try:
            while True:
                raw = command(["hyprctl", "-j", "cua:status"], timeout=2.0)
                summary = summarize_status(raw.get("json"))
                record = {"at": utc_now(), "status": raw, "summary": summary,
                          "processes": process_inventory()}
                stream.write(json.dumps(record) + "\n")
                stream.flush()
                samples += 1
                state = summary["state"]
                seen_states[state] = seen_states.get(state, 0) + 1
                fingerprint = json.dumps(summary, sort_keys=True)
                if fingerprint != previous:
                    print(record["at"], json.dumps(summary), flush=True)
                    previous = fingerprint
                remaining = deadline - time.monotonic()
                if args.watch == 0 or remaining <= 0:
                    break
                time.sleep(min(args.interval, remaining))
        except KeyboardInterrupt:
            interrupted = True
    write_json(output / "after.json", metadata())
    write_json(output / "summary.json", {"samples": samples, "states": seen_states, "interrupted": interrupted,
        "scope": "Status snapshots only. Not proof of client event delivery, overlay visibility, or freeze causation.",
        "privacy": "Local files may contain device names, paths, and layer metadata. Review before sharing. No chat logs, screenshots, environment dumps, or arbitrary process command lines collected."})
    print(f"Saved {samples} samples to {output}. Review before sharing.", flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
