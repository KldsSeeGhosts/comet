import { useApp } from "../store";
import type { EngineStatus } from "../../../preload/index";

/** Connection gate: every non-ready state is visible and honest about it. */
export function Gate({ conn }: { conn: EngineStatus }) {
  const retry = useApp((s) => s.retry);
  return (
    <div className="flex h-full items-center justify-center bg-bg text-ink">
      <div className="w-[360px] rounded-xl border border-line bg-surface p-6 text-center shadow-2xl">
        <div className="mx-auto mb-4 h-8 w-8 rounded-lg bg-accent-wash" />
        {conn.kind === "connecting" && (
          <>
            <div className="text-[14px] font-medium">Connecting to the engine</div>
            <div className="mt-1 text-[12px] text-ink-faint">
              Dialing the dev engine…
            </div>
          </>
        )}
        {conn.kind === "reconnecting" && (
          <>
            <div className="text-[14px] font-medium text-warning">
              Engine unreachable
            </div>
            <div className="mt-1 text-[12px] text-ink-faint">
              {conn.reason} — retry {conn.attempt} in{" "}
              {Math.round(conn.nextDelayMs / 1000)}s
            </div>
          </>
        )}
        {conn.kind === "failed" && (
          <>
            <div className="text-[14px] font-medium text-danger">
              Engine failed
            </div>
            <div className="mt-1 break-words text-[12px] text-ink-faint">
              {conn.error}
            </div>
          </>
        )}
        <div className="mt-5 flex items-center justify-center gap-2">
          <button
            onClick={retry}
            className="rounded-md border border-line-strong bg-overlay px-3 py-1.5 text-[12px] font-medium text-ink hover:bg-hover"
          >
            Retry now
          </button>
        </div>
        <div className="mt-4 text-[11px] text-ink-faint">
          Dev engine only — <span className="font-mono">ws://127.0.0.1:27655</span>,
          data <span className="font-mono">~/.zeron-dev</span>
        </div>
      </div>
    </div>
  );
}
