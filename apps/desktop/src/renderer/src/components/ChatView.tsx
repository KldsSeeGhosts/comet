import { useApp } from "../store";
import { Transcript } from "./Transcript";

/** Chat header — harness/model metadata + the transcript. */
export function ChatView({ chatId }: { chatId: string }) {
  const chat = useApp((s) => s.chats.find((c) => c.id === chatId));
  const view = useApp((s) => s.sessionViews[chatId]);
  const session = useApp((s) => s.sessions[chatId]);
  const harnesses = useApp((s) => s.harnesses);
  const transcriptError = useApp((s) => s.transcriptErrors[chatId]);

  const harness = harnesses.find(
    (h) => h.id === (view?.provider ?? chat?.config?.harness),
  );
  const model = view?.model ?? chat?.config?.model ?? null;

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <header className="flex h-11 shrink-0 items-center gap-2 border-b border-line bg-bg px-4">
        <div className="min-w-0 flex-1">
          <div className="truncate text-[13px] font-medium text-ink">
            {chat?.title?.trim() || "Session"}
          </div>
        </div>
        <div className="flex items-center gap-2 text-[11px] text-ink-faint">
          {session && session.status !== "idle" && (
            <span className="flex items-center gap-1.5">
              <span className="streaming-dot h-1.5 w-1.5 rounded-full bg-accent" />
              {session.status === "awaitingInput" ? "Awaiting input" : "Working"}
            </span>
          )}
          {harness && (
            <span className="rounded border border-line bg-card px-1.5 py-0.5 font-medium text-ink-muted">
              {harness.name}
            </span>
          )}
          {model && (
            <span className="max-w-[220px] truncate font-mono text-[10.5px] text-ink-faint">
              {model}
            </span>
          )}
          {view?.reasoning && (
            <span className="text-ink-faint">{view.reasoning}</span>
          )}
        </div>
      </header>
      {view?.error && (
        <div className="border-b border-line bg-card px-4 py-2 text-[12px] text-danger">
          {view.error}
        </div>
      )}
      {transcriptError && (
        <div className="border-b border-line bg-card px-4 py-2 text-[12px] text-warning">
          Transcript: {transcriptError}
        </div>
      )}
      <Transcript chatId={chatId} />
    </div>
  );
}
