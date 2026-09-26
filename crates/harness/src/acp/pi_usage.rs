//! Pi context-usage file watcher.
//!
//! pi-acp never emits ACP `usage_update`, so the Noches Pi extension
//! (`pi/noches-context-usage.ts`, loaded through the same wrapper as the CUA
//! extension) writes Pi's own `ctx.getContextUsage()` snapshot to
//! `NOCHES_PI_USAGE_FILE` on every session/turn boundary. This module polls
//! that file (mtime, ~300ms - cheap and portable, unlike a socket) and maps
//! each fresh snapshot onto [`AgentEvent::ContextUsage`], the same event the
//! other agents reach through their adapters' `usage_update`.
//!
//! `tokens: null` right after compaction is a real state - Pi reports
//! "unknown until the next assistant response" - and maps to
//! `ContextUsage { tokens: None, .. }` so the ring renders "waiting"
//! instead of a stale pre-compaction number.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::Deserialize;
use tokio::sync::mpsc;
use zeron_proto::{AgentEvent, ContextUsage, SessionTokenTotals};

/// Poll cadence. At ~300ms the ring feels live without measurable cost.
const POLL_INTERVAL: Duration = Duration::from_millis(300);

/// A parsed snapshot file. `percent`, `model` and `ts` ride along for
/// future card affordances but are ignored on the wire today. A snapshot
/// with no `contextWindow` yields no event (Pi reports nothing meaningful
/// without a model window).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Snapshot {
    /// `null` right after compaction until the next assistant response.
    tokens: Option<u64>,
    context_window: Option<u64>,
    compaction_percent: Option<f64>,
    session_tokens: Option<SessionTotals>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionTotals {
    input: u64,
    output: u64,
    cache_read: u64,
}

fn parse_snapshot(contents: &str) -> Option<Snapshot> {
    serde_json::from_str(contents).ok()
}

impl Snapshot {
    /// The file is authoritative and complete, so the event REPLACES the
    /// stored usage: `tokens: None` post-compaction must read as "waiting",
    /// not merge with the stale pre-compaction number.
    fn event(&self) -> Option<AgentEvent> {
        let window = self.context_window.filter(|w| *w > 0)?;
        Some(AgentEvent::ContextUsageSnapshot {
            usage: ContextUsage {
                tokens: self.tokens,
                window: Some(window),
                compaction_percent: self.compaction_percent.filter(|p| *p > 0.0),
                session: self.session_tokens.map(|s| SessionTokenTotals {
                    input: s.input,
                    output: s.output,
                    cache_read: s.cache_read,
                }),
            },
        })
    }
}

/// Poll the file until the run's event channel closes. Only a mtime change
/// re-reads and re-emits, so an idle session costs one `metadata` syscall per
/// tick and no events.
pub(crate) fn spawn_watcher(
    usage_file: PathBuf,
    event_tx: mpsc::Sender<Result<AgentEvent, crate::HarnessError>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut last: Option<(SystemTime, u64)> = None;
        let mut tick = tokio::time::interval(POLL_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            if event_tx.is_closed() {
                return;
            }
            let Some(stamp) = file_stamp(&usage_file) else {
                continue;
            };
            if last == Some(stamp) {
                continue;
            }
            last = Some(stamp);
            if let Some(ev) = read_event(&usage_file)
                && event_tx.send(Ok(ev)).await.is_err()
            {
                return;
            }
        }
    })
}

fn file_stamp(path: &Path) -> Option<(SystemTime, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    Some((mtime, meta.len()))
}

/// One non-timestamped read (final drain after the run settles, so the last
/// written snapshot still lands even if its mtime raced the last tick).
pub(crate) fn read_event(usage_file: &Path) -> Option<AgentEvent> {
    let contents = std::fs::read_to_string(usage_file).ok()?;
    parse_snapshot(&contents).and_then(|snap| snap.event())
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeron_proto::AgentEvent;

    fn write(path: &Path, body: &str) {
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn snapshot_maps_tokens_and_window() {
        let ev = parse_snapshot(
            r#"{"tokens":84213,"contextWindow":200000,"percent":42.1,"model":"cpa/gemini","ts":1}"#,
        )
        .unwrap()
        .event()
        .unwrap();
        assert!(matches!(
            ev,
            AgentEvent::ContextUsageSnapshot { usage }
                if usage.tokens == Some(84213) && usage.window == Some(200000)
        ));
    }

    #[test]
    fn snapshot_carries_compaction_and_session_totals() {
        let ev = parse_snapshot(
            r#"{"tokens":84213,"contextWindow":200000,"compactionPercent":91.8,"sessionTokens":{"input":50000,"output":10000,"cacheRead":40000}}"#,
        )
        .unwrap()
        .event()
        .unwrap();
        assert!(matches!(
            ev,
            AgentEvent::ContextUsageSnapshot { usage }
                if usage.compaction_percent == Some(91.8)
                    && usage.session
                        == Some(SessionTokenTotals {
                            input: 50000,
                            output: 10000,
                            cache_read: 40000,
                        })
        ));
    }

    #[test]
    fn post_compaction_null_tokens_become_waiting() {
        let ev = parse_snapshot(r#"{"tokens":null,"contextWindow":200000,"percent":null}"#)
            .unwrap()
            .event()
            .unwrap();
        assert!(matches!(
            ev,
            AgentEvent::ContextUsageSnapshot { usage }
                if usage.tokens.is_none() && usage.window == Some(200000)
        ));
    }

    #[test]
    fn malformed_and_windowless_snapshots_emit_nothing() {
        assert!(parse_snapshot("not json").is_none());
        assert!(parse_snapshot(r#"{"tokens":5}"#).unwrap().event().is_none());
        assert!(
            parse_snapshot(r#"{"tokens":5,"contextWindow":0}"#)
                .unwrap()
                .event()
                .is_none()
        );
    }

    #[tokio::test]
    async fn watcher_emits_once_per_write_and_ignores_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("context-usage.json");
        let (tx, mut rx) = mpsc::channel(16);
        let handle = spawn_watcher(file.clone(), tx);

        // Nothing yet: no file, no events.
        assert!(rx.try_recv().is_err());

        write(&file, r#"{"tokens":10,"contextWindow":1000}"#);
        let ev = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(matches!(
            ev,
            AgentEvent::ContextUsageSnapshot { usage }
                if usage.tokens == Some(10) && usage.window == Some(1000)
        ));

        // Same content rewritten with a new mtime still re-emits (fresh
        // snapshot), but an untouched file emits nothing across many ticks.
        std::thread::sleep(Duration::from_millis(50));
        write(&file, r#"{"tokens":10,"contextWindow":1000}"#);
        let ev = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(matches!(ev, AgentEvent::ContextUsageSnapshot { .. }));

        tokio::time::sleep(POLL_INTERVAL * 4).await;
        assert!(rx.try_recv().is_err(), "unchanged mtime must not re-emit");

        // Malformed content is ignored (a fresh mtime alone is not enough).
        write(&file, "garbage{{{");
        tokio::time::sleep(POLL_INTERVAL * 4).await;
        assert!(rx.try_recv().is_err(), "malformed file must be ignored");

        // A null-token snapshot is a real event, not silence.
        write(
            &file,
            r#"{"tokens":null,"contextWindow":1000,"percent":null}"#,
        );
        let ev = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(matches!(
            ev,
            AgentEvent::ContextUsageSnapshot { usage }
                if usage.tokens.is_none() && usage.window == Some(1000)
        ));

        handle.abort();
    }

    #[test]
    fn read_event_reads_latest_snapshot_directly() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("context-usage.json");
        assert!(read_event(&file).is_none());
        write(&file, r#"{"tokens":42,"contextWindow":200000}"#);
        assert!(matches!(
            read_event(&file),
            Some(AgentEvent::ContextUsageSnapshot { usage })
                if usage.tokens == Some(42) && usage.window == Some(200000)
        ));
    }
}
