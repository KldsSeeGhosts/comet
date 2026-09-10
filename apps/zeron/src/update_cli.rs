//! `zeron update` — disabled in fork builds.
//!
//! Upstream this performed the managed-install flow from `edge/src/install.sh`
//! (download → verify → symlink swap → service restart). In this fork the
//! upstream release channel is intentionally disconnected: an update pulled
//! from `{edge}/releases` would silently replace a fork build with the
//! upstream binary. Update a fork build with `git pull` + rebuild instead.

use anyhow::bail;

/// `--check` prints the verdict and exits (nonzero when an update is available,
/// so scripts can gate on it).
pub async fn update(_edge_url: &str, _check_only: bool) -> anyhow::Result<()> {
    bail!(
        "upstream self-update is disabled in this fork build — update via `git pull` + rebuild from your fork instead."
    )
}
