#!/usr/bin/env python3
"""Apply the exact-package Zen background-input qualification.

A checkout that already carries the upstream-committed entry is detected from
semantic markers, so `--check` and apply become no-ops there. Partial
integration fails closed. Older unintegrated baselines still go through the
patch below. When an install record exists but its hash no longer matches, the
mismatch is only accepted as upstream integration if the file is tracked and
clean at git HEAD; a local edit made after installation is never silently
accepted.
"""
from pathlib import Path
import argparse
import hashlib
import json
import subprocess

HERE = Path(__file__).resolve().parent
PATCH = HERE / "zen-background.patch"
FILE = "libs/cua-driver/rust/crates/platform-linux/src/wayland/hyprland_compatibility.rs"
# Whitespace is ignored so rustfmt reflows do not look like drift.
INTEGRATION_MARKERS = (
    'QualifiedPackage { name: "zen-browser-bin", version: "1.22b-1", executable: "/opt/zen-browser-bin/zen-bin", }',
    "fn zen_admission_is_pinned_to_the_tested_package()",
    'assert_eq!(zen.version, "1.22b-1");',
    'assert_eq!(zen.executable, "/opt/zen-browser-bin/zen-bin");',
)


def _compacted(text: str) -> str:
    return "".join(text.split())


def integration_state(root: Path) -> str:
    """Return "full", "none", or "partial" for the Zen qualification."""
    path = root / FILE
    if not path.is_file():
        return "none"
    text = _compacted(path.read_text())
    present = [marker for marker in INTEGRATION_MARKERS if _compacted(marker) in text]
    if len(present) == len(INTEGRATION_MARKERS):
        return "full"
    return "none" if not present else "partial"


def tracked_clean_at_head(root: Path, paths: tuple[str, ...]) -> bool:
    """True when every path is tracked and unmodified relative to git HEAD."""
    commands = (
        ["git", "-C", str(root), "ls-files", "--error-unmatch", "--", *paths],
        ["git", "-C", str(root), "diff", "--quiet", "HEAD", "--", *paths],
    )
    for command in commands:
        try:
            result = subprocess.run(command, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        except OSError:
            return False
        if result.returncode != 0:
            return False
    return True


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
    state = integration_state(root)
    if state == "partial":
        raise SystemExit("Zen background repair is only partially integrated; reconcile manually")
    integrated = state == "full"
    if stamp.exists():
        record = json.loads(stamp.read_text())
        if not (root / FILE).is_file() or digest(root / FILE) != record["digest"]:
            if integrated and tracked_clean_at_head(root, (FILE,)):
                print("Cua checkout has the upstream-committed Zen entry; stale install record ignored")
                return
            raise SystemExit(f"Patched file changed since install: {FILE}; reconcile manually")
        if not integrated:
            raise SystemExit("Install record hash matches, but the Zen repair is not fully integrated; reconcile manually")
        print("Zen background repair already installed; hash verified")
        return
    if integrated:
        print("Cua checkout already contains the Zen entry; nothing to apply")
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
