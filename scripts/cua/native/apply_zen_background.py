#!/usr/bin/env python3
"""Apply the exact-package Zen background-input qualification."""
from pathlib import Path
import argparse
import hashlib
import json
import subprocess

HERE = Path(__file__).resolve().parent
PATCH = HERE / "zen-background.patch"
FILE = "libs/cua-driver/rust/crates/platform-linux/src/wayland/hyprland_compatibility.rs"


def digest(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def run(root: Path, *args: str, check: bool = True) -> subprocess.CompletedProcess:
    return subprocess.run(["git", "-C", str(root), *args], check=check, capture_output=True, text=True)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("checkout", type=Path)
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    root = args.checkout.resolve()
    stamp = root / ".git/noches-cua-zen-background.json"
    if not (root / ".git").is_dir():
        raise SystemExit("Expected an ordinary Cua Git checkout, not a linked worktree")
    if stamp.exists():
        record = json.loads(stamp.read_text())
        if digest(root / FILE) != record["digest"]:
            raise SystemExit(f"Patched file changed since install: {FILE}; reconcile manually")
        print("Zen background repair already installed; hash verified")
        return
    forward = run(root, "apply", "--check", str(PATCH), check=False)
    already_applied = False
    if forward.returncode != 0:
        reverse = run(root, "apply", "--reverse", "--check", str(PATCH), check=False)
        if reverse.returncode != 0:
            raise SystemExit(forward.stderr.strip() or "Zen background patch does not apply")
        already_applied = True
    if args.check:
        print("Zen background repair is already present" if already_applied else "Zen background repair applies cleanly")
        return
    if not already_applied:
        run(root, "apply", str(PATCH))
    stamp.write_text(json.dumps({"file": FILE, "digest": digest(root / FILE)}, indent=2) + "\n")
    print("Recorded Zen background repair" if already_applied else "Applied Zen background repair")


if __name__ == "__main__":
    main()
