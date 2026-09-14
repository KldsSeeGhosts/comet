import { useRef, useState } from "react";

import { useApp } from "../store";
import { effectiveIndicator, sendButtonMode } from "../../../shared/view";
import { steersMidTurn } from "../../../shared/types";

/** Composer — Enter sends, Shift+Enter newlines; while a run is live the
 *  button morphs Send → Queue (with text) or Stop (empty). */
export function Composer({ chatId }: { chatId: string }) {
  const [text, setText] = useState("");
  const send = useApp((s) => s.send);
  const interrupt = useApp((s) => s.interrupt);
  const sendError = useApp((s) => s.sendError);
  const sending = useApp((s) => s.sending);
  const session = useApp((s) => s.sessions[chatId]);
  const chat = useApp((s) => s.chats.find((c) => c.id === chatId));
  const view = useApp((s) => s.sessionViews[chatId]);
  const harnesses = useApp((s) => s.harnesses);
  const usage = useApp((s) => s.transcripts[chatId]?.contextUsage);
  const now = useApp((s) => s.now);
  const taRef = useRef<HTMLTextAreaElement>(null);

  const indicator = effectiveIndicator(session, new Date(now));
  const live = indicator === "working" || indicator === "awaitingInput";
  const mode = sendButtonMode(live, text.trim().length > 0);
  const descriptor = harnesses.find(
    (h) => h.id === (view?.provider ?? chat?.config?.harness),
  );

  const submit = () => {
    const value = text.trim();
    if (mode === "stop") {
      void interrupt();
      return;
    }
    if (!value) return;
    setText("");
    void send(value).catch(() => {
      // sendError is already in the store; restore the draft.
      setText(value);
    });
    requestAnimationFrame(() => taRef.current?.focus());
  };

  const onKeyDown = (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
    if (e.key === "Enter" && !e.shiftKey && !e.nativeEvent.isComposing) {
      e.preventDefault();
      submit();
    }
  };

  const autogrow = () => {
    const ta = taRef.current;
    if (!ta) return;
    ta.style.height = "auto";
    ta.style.height = Math.min(ta.scrollHeight, 200) + "px";
  };

  return (
    <div className="shrink-0 border-t border-line bg-bg px-4 pb-3 pt-2">
      <div className="mx-auto max-w-[760px]">
        {sendError && (
          <div className="mb-2 rounded-md border border-danger/40 bg-danger/10 px-3 py-1.5 text-[12px] text-danger">
            {sendError}
          </div>
        )}
        <div className="rounded-xl border border-line-strong bg-input shadow-lg focus-within:border-accent/60">
          <textarea
            ref={taRef}
            value={text}
            onChange={(e) => {
              setText(e.target.value);
              autogrow();
            }}
            onKeyDown={onKeyDown}
            rows={2}
            placeholder={
              live
                ? "Queue a message for when the agent finishes…"
                : "Message the agent…"
            }
            className="block w-full resize-none bg-transparent px-3.5 pb-1 pt-3 text-[13px] text-ink placeholder:text-ink-faint focus:outline-none"
          />
          <div className="flex items-center gap-2 px-2.5 pb-2">
            <div className="min-w-0 flex-1 truncate text-[10.5px] text-ink-faint">
              {descriptor && (
                <span className="font-medium text-ink-muted">
                  {descriptor.name}
                </span>
              )}
              {(view?.model ?? chat?.config?.model) && (
                <span className="ml-1.5 font-mono">
                  {view?.model ?? chat?.config?.model}
                </span>
              )}
              {descriptor && steersMidTurn(descriptor) && (
                <span className="ml-1.5">· steerable</span>
              )}
            </div>
            {usage?.tokens != null && usage.window != null && usage.window > 0 && (
              <div
                className="shrink-0 text-[10px] text-ink-faint"
                title={`${usage.tokens.toLocaleString()} / ${usage.window.toLocaleString()} tokens`}
              >
                {Math.round((usage.tokens / usage.window) * 100)}% ctx
              </div>
            )}
            <button
              onClick={submit}
              disabled={sending}
              className={`shrink-0 rounded-md px-3 py-1.5 text-[12px] font-medium ${
                mode === "stop"
                  ? "border border-danger/50 bg-danger/15 text-danger hover:bg-danger/25"
                  : mode === "queue"
                    ? "border border-line-strong bg-overlay text-ink hover:bg-hover"
                    : "bg-accent-strong text-on-accent hover:bg-accent disabled:opacity-40"
              }`}
            >
              {mode === "stop" ? "■ Stop" : mode === "queue" ? "Queue" : "Send"}
            </button>
          </div>
        </div>
        <div className="mt-1.5 text-center text-[10px] text-ink-faint">
          Enter sends · while running, messages queue on the chat doc · runs
          survive renderer reloads
        </div>
      </div>
    </div>
  );
}
