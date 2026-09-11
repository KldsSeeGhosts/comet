#!/usr/bin/env python3
"""Make Hyprland IPC and Cua input survive a compositor relogin."""
from pathlib import Path
import argparse
import hashlib
import json
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
    if stamp.exists():
        record = json.loads(stamp.read_text())
        for relative, digest in record["files"].items():
            if hashlib.sha256((root / relative).read_bytes()).hexdigest() != digest:
                raise SystemExit(f"Patched file changed since install: {relative}; reconcile manually")
        print("Hyprland runtime repair already installed; hashes verified")
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
