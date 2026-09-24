#!/usr/bin/env python3
"""Allow independent-seat input to another top-level of the same client.

A checkout that already carries the upstream-committed repair is detected from
semantic markers, so `--check` and apply become no-ops there. Partial
integration fails closed. Older unintegrated baselines still go through the
reviewed patch below. When an install record exists but its hash no longer
matches, the mismatch is only accepted as upstream integration if the file is
tracked and clean at git HEAD; a local edit made after installation is never
silently accepted.
"""
from pathlib import Path
import argparse
import hashlib
import json
import subprocess

HERE = Path(__file__).resolve().parent
PATCH = HERE / "hyprland-same-client.patch"
FILE = "libs/cua-driver/hyprland-plugin/src/input_experiment.cpp"
# Whitespace is ignored so clang-format reflows do not look like drift. The
# forbidden fragments catch a checkout that kept the per-client refusal.
INTEGRATION_MARKERS = (
    "return pointer == surface || keyboard == surface;",
    "A client may own several top-levels.",
)
FORBIDDEN_MARKERS = (
    "return (pointer && pointer->client() == surface->client()) || (keyboard && keyboard->client() == surface->client());",
    "Conservative per-client refusal avoids toolkit-global cross-window state.",
)


def digest(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def _compacted(text: str) -> str:
    return "".join(text.split())


def integration_state(root: Path) -> str:
    """Return "full", "none", or "partial" for the same-client repair."""
    path = root / FILE
    if not path.is_file():
        return "none"
    text = _compacted(path.read_text())
    present = [marker for marker in INTEGRATION_MARKERS if _compacted(marker) in text]
    stale = [marker for marker in FORBIDDEN_MARKERS if _compacted(marker) in text]
    if len(present) == len(INTEGRATION_MARKERS) and not stale:
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
    state = integration_state(root)
    if state == "partial":
        raise SystemExit("Hyprland same-client repair is only partially integrated; reconcile manually")
    integrated = state == "full"
    if stamp.exists():
        record = json.loads(stamp.read_text())
        if not (root / FILE).is_file() or digest(root / FILE) != record["digest"]:
            if integrated and tracked_clean_at_head(root, (FILE,)):
                print("Cua checkout has the upstream-committed same-client repair; stale install record ignored")
                return
            raise SystemExit(f"Patched file changed since install: {FILE}; reconcile manually")
        if not integrated:
            raise SystemExit("Install record hash matches, but the same-client repair is not fully integrated; reconcile manually")
        if not args.check:
            refresh_parent_stamp(root)
        print("Hyprland same-client repair already installed; hash verified")
        return
    if integrated:
        print("Cua checkout already contains the same-client repair; nothing to apply")
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
