import { useState } from "react";

import { useApp } from "../store";
import type { UserInputQuestion } from "../../../shared/types";

/** Agent input request — options click straight through to RespondInput. */
export function InputPanel({
  requestId,
  questions,
}: {
  requestId: string;
  questions: UserInputQuestion[];
}) {
  const respondInput = useApp((s) => s.respondInput);
  const [picks, setPicks] = useState<Record<string, string[]>>({});
  const [sending, setSending] = useState(false);

  const answer = (questionId: string, labels: string[]) => {
    if (sending) return;
    setSending(true);
    void respondInput(requestId, [{ questionId, labels }]).finally(() =>
      setSending(false),
    );
  };

  const toggle = (q: UserInputQuestion, label: string) => {
    if (!q.multiSelect) {
      answer(q.id, [label]);
      return;
    }
    setPicks((p) => {
      const cur = p[q.id] ?? [];
      return {
        ...p,
        [q.id]: cur.includes(label)
          ? cur.filter((l) => l !== label)
          : [...cur, label],
      };
    });
  };

  return (
    <div className="my-2 rounded-lg border border-warning/40 bg-card px-3 py-2.5">
      <div className="text-[11.5px] font-medium text-warning">
        The agent needs input
      </div>
      {questions.map((q) => (
        <div key={q.id} className="mt-2">
          <div className="text-[12px] text-ink">
            <span className="text-ink-faint">{q.header} — </span>
            {q.question}
          </div>
          <div className="mt-1.5 flex flex-wrap gap-1.5">
            {q.options.map((label) => {
              const picked = picks[q.id]?.includes(label);
              return (
                <button
                  key={label}
                  disabled={sending}
                  onClick={() => toggle(q, label)}
                  className={`rounded-md border px-2.5 py-1 text-[11.5px] ${
                    picked
                      ? "border-accent bg-accent-wash text-ink"
                      : "border-line bg-overlay text-ink-muted hover:bg-hover hover:text-ink"
                  }`}
                >
                  {label}
                </button>
              );
            })}
            {q.multiSelect && (
              <button
                disabled={sending || !(picks[q.id]?.length ?? 0)}
                onClick={() => answer(q.id, picks[q.id] ?? [])}
                className="rounded-md border border-accent bg-accent-strong px-2.5 py-1 text-[11.5px] font-medium text-on-accent disabled:opacity-40"
              >
                Send
              </button>
            )}
          </div>
        </div>
      ))}
    </div>
  );
}
