import { useApp } from "../store";
import { steersMidTurn } from "../../../shared/types";

/** The durable message queue for this chat — rows the host will promote at
 *  turn end, or immediately via Send now / Steer (when the harness can take
 *  a mid-turn steer). */
const EMPTY_QUEUE: import("../../../shared/types").QueuedMessage[] = [];

export function QueuePanel({ chatId }: { chatId: string }) {
  const items = useApp((s) => s.queues[chatId] ?? EMPTY_QUEUE);
  const chat = useApp((s) => s.chats.find((c) => c.id === chatId));
  const view = useApp((s) => s.sessionViews[chatId]);
  const harnesses = useApp((s) => s.harnesses);
  const steerQueued = useApp((s) => s.steerQueued);
  const sendQueuedNow = useApp((s) => s.sendQueuedNow);
  const removeQueued = useApp((s) => s.removeQueued);

  if (items.length === 0) return null;

  const descriptor = harnesses.find(
    (h) => h.id === (view?.provider ?? chat?.config?.harness),
  );
  const canSteer = descriptor ? steersMidTurn(descriptor) : false;

  return (
    <div className="border-t border-line bg-surface px-4 py-2">
      <div className="mx-auto max-w-[760px]">
        <div className="pb-1 text-[10.5px] font-medium uppercase tracking-wider text-ink-faint">
          Queued · {items.length}
        </div>
        {items.map((item) => (
          <div
            key={item.id}
            className="group flex items-center gap-2 rounded-md px-1 py-1 hover:bg-hover"
          >
            <div className="min-w-0 flex-1 truncate text-[12px] text-ink-muted">
              {item.text}
            </div>
            <div className="flex shrink-0 items-center gap-1 opacity-0 group-hover:opacity-100">
              {canSteer && (
                <button
                  onClick={() => void steerQueued(item.id)}
                  title="Steer into the live turn"
                  className="rounded border border-line px-1.5 py-0.5 text-[10.5px] text-ink-muted hover:bg-active hover:text-ink"
                >
                  Steer
                </button>
              )}
              <button
                onClick={() => void sendQueuedNow(item.id)}
                title="Interrupt and send now"
                className="rounded border border-line px-1.5 py-0.5 text-[10.5px] text-ink-muted hover:bg-active hover:text-ink"
              >
                Send now
              </button>
              <button
                onClick={() => void removeQueued(item.id)}
                title="Remove"
                className="rounded border border-line px-1.5 py-0.5 text-[10.5px] text-ink-faint hover:bg-active hover:text-danger"
              >
                ✕
              </button>
            </div>
          </div>
        ))}
      </div>
    </div>
  );
}
