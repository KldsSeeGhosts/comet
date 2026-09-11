#!/usr/bin/env python3
"""Apply the reviewed duplicate-primary-seat repair to a Cua checkout.

A checkout that already carries the upstream-committed repair is detected from
semantic markers, so `--check` and apply become no-ops there. Partial
integration fails closed. Older unintegrated baselines still go through the
reviewed patch below. When an install record exists but its hashes no longer
match, the mismatch is only accepted as upstream integration if every recorded
file is tracked and clean at git HEAD; a local edit made after installation is
never silently accepted.
"""
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
# Each group must match as a whole. Whitespace is ignored so clang-format
# reflows and equivalent formatting do not look like drift. Forbidden fragments
# catch a checkout that kept the exact-one-seat rule next to the new one.
INTEGRATION_GROUPS = (
    (FILES[0], (
        'void observe(std::string_view resource_class, bool plugin_owned) { if (resource_class == "wl_seat" && !plugin_owned) ++primary_candidates; }',
        "bool available() const { return primary_candidates > 0; }",
    ), (
        "bool unique() const { return primary_candidates == 1; }",
        "primary_candidates < 2",
    )),
    (FILES[1], (
        "std::vector<WP<CWLSeatResource>> primary_seats(wl_client* client) const",
        "bool primary_seats_stable(wl_client* client) const",
        "std::vector<WP<CWLSeatResource>> foreground_seats;",
        ".primary_binding = !exact_root || primary_seats_stable(root->client()),",
        "foreground_seats = seats;",
        "foreground_seats.clear();",
    ), (
        "WP<CWLSeatResource> foreground_seat;",
        "bool unique_primary_seat(wl_client* client) const",
    )),
    (FILES[2], (
        "check(bindings.available());",
        "// Firefox and derivatives may bind the same primary global more than once.",
    ), (
        "bindings.unique()",
    )),
)


def digest(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def _compacted(text: str) -> str:
    return "".join(text.split())


def integration_state(root: Path) -> tuple[str, str]:
    """Return ("full" | "none" | "partial", detail) for the duplicate-seat repair."""
    complete, partial, absent = [], [], []
    for relative, required, forbidden in INTEGRATION_GROUPS:
        path = root / relative
        if not path.is_file():
            absent.append(relative)
            continue
        text = _compacted(path.read_text())
        present = [marker for marker in required if _compacted(marker) in text]
        stale = [marker for marker in forbidden if _compacted(marker) in text]
        if len(present) == len(required) and not stale:
            complete.append(relative)
        elif not present:
            absent.append(relative)
        else:
            partial.append(relative)
    if complete and not partial and not absent:
        return "full", ""
    if not complete and not partial:
        return "none", ""
    detail = []
    if partial:
        detail.append("incomplete marker sets in " + ", ".join(partial))
    if complete and absent:
        detail.append("missing or unmarked " + ", ".join(absent))
    return "partial", "; ".join(detail)


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
    state, detail = integration_state(root)
    if state == "partial":
        raise SystemExit(f"Hyprland duplicate-seat repair is only partially integrated ({detail}); reconcile manually")
    integrated = state == "full"
    if stamp.exists():
        record = json.loads(stamp.read_text())
        changed = [relative for relative, expected in record["files"].items()
                   if not (root / relative).is_file() or digest(root / relative) != expected]
        if changed:
            if integrated and tracked_clean_at_head(root, tuple(record["files"])):
                print("Cua checkout has the upstream-committed duplicate-seat repair; stale install record ignored")
                return
            raise SystemExit(f"Patched file changed since install: {changed[0]}; reconcile manually")
        if not integrated:
            raise SystemExit("Install record hashes match, but the duplicate-seat repair is not fully integrated; reconcile manually")
        print("Hyprland duplicate-seat repair already installed; hashes verified")
        return
    if integrated:
        print("Cua checkout already contains the duplicate-seat repair; nothing to apply")
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
