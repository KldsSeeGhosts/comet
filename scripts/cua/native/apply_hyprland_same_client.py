#!/usr/bin/env python3
"""Allow independent-seat input to another top-level of the same client."""
from pathlib import Path
import argparse
import hashlib
import json
import subprocess

HERE = Path(__file__).resolve().parent
PATCH = HERE / "hyprland-same-client.patch"
FILE = "libs/cua-driver/hyprland-plugin/src/input_experiment.cpp"


def digest(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def run(root: Path, *args: str) -> subprocess.CompletedProcess:
    return subprocess.run(["git", "-C", str(root), *args], capture_output=True, text=True)


def refresh_parent_stamp(root: Path) -> None:
    stamp = root / ".git/noches-cua-hyprland-multibind.json"
    if not stamp.exists():
        return
    record = json.loads(stamp.read_text())
    if FILE in record.get("files", {}):
        record["files"][FILE] = digest(root / FILE)
        stamp.write_text(json.dumps(record, indent=2) + "\n")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("checkout", type=Path)
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    root = args.checkout.resolve()
    stamp = root / ".git/noches-cua-hyprland-same-client.json"
    if not (root / ".git").is_dir():
        raise SystemExit("Expected an ordinary Cua Git checkout, not a linked worktree")
    if stamp.exists():
        record = json.loads(stamp.read_text())
        if digest(root / FILE) != record["digest"]:
            raise SystemExit(f"Patched file changed since install: {FILE}; reconcile manually")
        refresh_parent_stamp(root)
        print("Hyprland same-client repair already installed; hash verified")
        return
    forward = run(root, "apply", "--check", str(PATCH))
    already_applied = False
    if forward.returncode != 0:
        reverse = run(root, "apply", "--reverse", "--check", str(PATCH))
        if reverse.returncode != 0:
            raise SystemExit(forward.stderr.strip() or "Hyprland same-client patch does not apply")
        already_applied = True
    if args.check:
        print("Hyprland same-client repair is already present" if already_applied else "Hyprland same-client repair applies cleanly")
        return
    if not already_applied:
        applied = run(root, "apply", str(PATCH))
        if applied.returncode != 0:
            raise SystemExit(applied.stderr.strip())
    stamp.write_text(json.dumps({"file": FILE, "digest": digest(root / FILE)}, indent=2) + "\n")
    refresh_parent_stamp(root)
    print("Recorded Hyprland same-client repair" if already_applied else "Applied Hyprland same-client repair")


if __name__ == "__main__":
    main()
