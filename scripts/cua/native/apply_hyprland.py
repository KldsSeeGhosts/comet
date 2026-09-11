#!/usr/bin/env python3
"""Apply the reviewed duplicate-primary-seat repair to a Cua checkout."""
from pathlib import Path
import argparse
import hashlib
import json
import subprocess

HERE = Path(__file__).resolve().parent
PATCH = HERE / "hyprland-multibind.patch"
FILES = (
    "libs/cua-driver/hyprland-plugin/src/foreground_route.hpp",
    "libs/cua-driver/hyprland-plugin/src/input_experiment.cpp",
    "libs/cua-driver/hyprland-plugin/tests/foreground_route_test.cpp",
)


def digest(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def run(root: Path, *args: str) -> None:
    subprocess.run(["git", "-C", str(root), *args], check=True)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("checkout", type=Path)
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    root = args.checkout.resolve()
    stamp = root / ".git/noches-cua-hyprland-multibind.json"
    if not (root / ".git").is_dir():
        raise SystemExit("Expected an ordinary Cua Git checkout, not a linked worktree")
    if stamp.exists():
        record = json.loads(stamp.read_text())
        for relative, expected in record["files"].items():
            if digest(root / relative) != expected:
                raise SystemExit(f"Patched file changed since install: {relative}; reconcile manually")
        print("Hyprland duplicate-seat repair already installed; hashes verified")
        return
    run(root, "apply", "--check", str(PATCH))
    if args.check:
        print(f"Hyprland duplicate-seat repair applies cleanly to {len(FILES)} files")
        return
    run(root, "apply", str(PATCH))
    stamp.write_text(json.dumps({"files": {name: digest(root / name) for name in FILES}}, indent=2) + "\n")
    print("Applied Hyprland duplicate-seat repair")


if __name__ == "__main__":
    main()
