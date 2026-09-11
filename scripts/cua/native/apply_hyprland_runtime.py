#!/usr/bin/env python3
"""Make Hyprland IPC and Cua input survive a compositor relogin.

A checkout that already carries the upstream-committed repair is detected from
semantic markers, so `--check` and apply become no-ops there. Partial
integration fails closed. Older unintegrated baselines still go through the
anchor plan below. When an install record exists but its hashes no longer
match, the mismatch is only accepted as upstream integration if every recorded
file is tracked and clean at git HEAD; a local edit made after installation is
never silently accepted.
"""
from pathlib import Path
import argparse
import hashlib
import json
import subprocess
import tempfile


OLD_IPC = '''fn ipc_path() -> Result<PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").context("missing XDG_RUNTIME_DIR")?;
    let signature = std::env::var("HYPRLAND_INSTANCE_SIGNATURE")
        .context("missing HYPRLAND_INSTANCE_SIGNATURE")?;
    if signature.is_empty()
        || !signature
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        bail!("invalid Hyprland instance signature");
    }
    let runtime = PathBuf::from(runtime);
    if !runtime.is_absolute() {
        bail!("XDG_RUNTIME_DIR must be absolute");
    }
    Ok(runtime.join("hypr").join(signature).join(".socket.sock"))
}
'''

NEW_IPC = '''fn hyprland_runtime_root() -> Result<PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").context("missing XDG_RUNTIME_DIR")?;
    let runtime = PathBuf::from(runtime);
    if !runtime.is_absolute() {
        bail!("XDG_RUNTIME_DIR must be absolute");
    }
    Ok(runtime.join("hypr"))
}

fn valid_instance_signature(signature: &str) -> bool {
    !signature.is_empty()
        && signature != "."
        && signature != ".."
        && signature
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

fn instance_candidates() -> Result<Vec<PathBuf>> {
    let root = hyprland_runtime_root()?;
    let mut candidates = Vec::new();
    if let Ok(signature) = std::env::var("HYPRLAND_INSTANCE_SIGNATURE") {
        if valid_instance_signature(&signature) {
            candidates.push(root.join(signature));
        }
    }
    let mut discovered = std::fs::read_dir(&root)
        .with_context(|| format!("cannot inspect {}", root.display()))?
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_str()?;
            valid_instance_signature(name).then(|| entry.path())
        })
        .collect::<Vec<_>>();
    discovered.sort();
    for candidate in discovered {
        if !candidates.contains(&candidate) {
            candidates.push(candidate);
        }
    }
    anyhow::ensure!(candidates.len() <= 16, "too many Hyprland instance candidates");
    Ok(candidates)
}

/// Resolve the instance directory by matching the IPC socket peer to the
/// compositor that owns WAYLAND_DISPLAY. This survives a long-running user
/// service inheriting an obsolete HYPRLAND_INSTANCE_SIGNATURE after relogin.
pub(super) fn active_instance_dir_with_timeout(timeout: Duration) -> Result<PathBuf> {
    let wayland = wayland_connection_with_timeout(timeout)?;
    let compositor_pid = peer_pid(wayland.backend().poll_fd().as_raw_fd())?;
    let mut matched = Vec::new();
    for candidate in instance_candidates()? {
        let socket = socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None)?;
        if socket
            .connect_timeout(&socket2::SockAddr::unix(candidate.join(".socket.sock"))?, timeout)
            .is_ok()
            && peer_pid(socket.as_raw_fd()).ok() == Some(compositor_pid)
        {
            matched.push(candidate);
        }
    }
    match matched.as_slice() {
        [instance] => Ok(instance.clone()),
        [] => bail!("no Hyprland IPC socket belongs to the WAYLAND_DISPLAY compositor"),
        _ => bail!("multiple Hyprland IPC sockets belong to the WAYLAND_DISPLAY compositor"),
    }
}
'''

OLD_CONNECT = '''fn ipc_connection_with_timeout(timeout: Duration) -> Result<UnixStream> {
    let socket = socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None)?;
    socket.connect_timeout(&socket2::SockAddr::unix(ipc_path()?)?, timeout)?;
    Ok(socket.into())
}
'''

NEW_CONNECT = '''fn ipc_connection_with_timeout(timeout: Duration) -> Result<UnixStream> {
    let path = active_instance_dir_with_timeout(timeout)?.join(".socket.sock");
    let socket = socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None)?;
    socket.connect_timeout(&socket2::SockAddr::unix(path)?, timeout)?;
    Ok(socket.into())
}
'''

OLD_INPUT = '''fn socket_path(lane: usize) -> Result<PathBuf> {
    let runtime =
        PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").context("missing XDG_RUNTIME_DIR")?);
    let signature =
        std::env::var("HYPRLAND_INSTANCE_SIGNATURE").context("missing Hyprland instance")?;
    ensure!(runtime.is_absolute(), "XDG_RUNTIME_DIR must be absolute");
    ensure!(
        !signature.is_empty()
            && signature
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
            && signature != "."
            && signature != "..",
        "invalid Hyprland instance"
    );
    Ok(runtime
        .join("hypr")
        .join(signature)
        .join(protocol().socket_name(lane)?))
}
'''

NEW_INPUT = '''fn socket_path(lane: usize) -> Result<PathBuf> {
    Ok(super::hyprland::active_instance_dir_with_timeout(TIMEOUT)?
        .join(protocol().socket_name(lane)?))
}
'''

# Each group must match as a whole. Whitespace is ignored so rustfmt reflows
# and equivalent formatting do not look like drift. Forbidden fragments catch
# a checkout that kept the old resolver next to the new one.
INTEGRATION_GROUPS = (
    ("libs/cua-driver/rust/crates/platform-linux/src/wayland/hyprland.rs", (
        "fn hyprland_runtime_root() -> Result<PathBuf>",
        "fn valid_instance_signature(signature: &str) -> bool",
        "fn instance_candidates() -> Result<Vec<PathBuf>>",
        "too many Hyprland instance candidates",
        "pub(super) fn active_instance_dir_with_timeout(timeout: Duration) -> Result<PathBuf>",
        "no Hyprland IPC socket belongs to the WAYLAND_DISPLAY compositor",
        "multiple Hyprland IPC sockets belong to the WAYLAND_DISPLAY compositor",
        'let path = active_instance_dir_with_timeout(timeout)?.join(".socket.sock");',
    ), (
        "fn ipc_path() -> Result<PathBuf>",
        'Ok(runtime.join("hypr").join(signature).join(".socket.sock"))',
    )),
    ("libs/cua-driver/rust/crates/platform-linux/src/wayland/hyprland_input.rs", (
        "Ok(super::hyprland::active_instance_dir_with_timeout(TIMEOUT)?.join(protocol().socket_name(lane)?))",
    ), (
        'Ok(runtime.join("hypr").join(signature).join(protocol().socket_name(lane)?))',
    )),
)


def _compacted(text: str) -> str:
    return "".join(text.split())


def integration_state(root: Path) -> tuple[str, str]:
    """Return ("full" | "none" | "partial", detail) for the runtime repair."""
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


def transform(source: str, old: str, new: str) -> str:
    if source.count(new) == 1 and source.count(old) == 0:
        return source
    if source.count(old) != 1 or source.count(new) != 0:
        raise ValueError(f"Source drift or partial runtime repair near {old[:80]!r}")
    return source.replace(old, new, 1)


def plan(root: Path) -> dict[Path, str]:
    base = root / "libs/cua-driver/rust/crates/platform-linux/src/wayland"
    hyprland = base / "hyprland.rs"
    source = transform(hyprland.read_text(), OLD_IPC, NEW_IPC)
    source = transform(source, OLD_CONNECT, NEW_CONNECT)
    hyprland_input = base / "hyprland_input.rs"
    input_source = transform(hyprland_input.read_text(), OLD_INPUT, NEW_INPUT)
    return {hyprland: source, hyprland_input: input_source}


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("checkout", type=Path)
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    root = args.checkout.resolve()
    stamp = root / ".git/noches-cua-hyprland-runtime.json"
    if not (root / ".git").is_dir():
        raise SystemExit("Expected an ordinary Cua Git checkout, not a linked worktree")
    state, detail = integration_state(root)
    if state == "partial":
        raise SystemExit(f"Hyprland runtime repair is only partially integrated ({detail}); reconcile manually")
    integrated = state == "full"
    if stamp.exists():
        record = json.loads(stamp.read_text())
        changed = [relative for relative, digest in record["files"].items()
                   if not (root / relative).is_file()
                   or hashlib.sha256((root / relative).read_bytes()).hexdigest() != digest]
        if changed:
            if integrated and tracked_clean_at_head(root, tuple(record["files"])):
                print("Cua checkout has the upstream-committed runtime repair; stale install record ignored")
                return
            raise SystemExit(f"Patched file changed since install: {changed[0]}; reconcile manually")
        if not integrated:
            raise SystemExit("Install record hashes match, but the runtime repair is not fully integrated; reconcile manually")
        print("Hyprland runtime repair already installed; hashes verified")
        return
    if integrated:
        print("Cua checkout already contains the runtime repair; nothing to apply")
        return
    changes = plan(root)
    if args.check:
        print(f"All anchors checked; {len(changes)} files would change")
        return
    backup = Path(tempfile.mkdtemp(prefix="noches-hyprland-runtime-before-", dir=root / ".git"))
    before = {path: path.read_bytes() for path in changes}
    try:
        for path, source in changes.items():
            target = backup / path.relative_to(root)
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(before[path])
            path.write_text(source)
        stamp.write_text(json.dumps({"backup": str(backup), "files": {
            str(path.relative_to(root)): hashlib.sha256(path.read_bytes()).hexdigest()
            for path in changes
        }}, indent=2) + "\n")
    except BaseException:
        for path, data in before.items():
            path.write_bytes(data)
        stamp.unlink(missing_ok=True)
        raise
    print(f"Applied Hyprland runtime repair; originals: {backup}")


if __name__ == "__main__":
    main()
