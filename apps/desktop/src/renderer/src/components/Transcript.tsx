import { useEffect, useRef, useState } from "react";
import Markdown from "react-markdown";
import remarkGfm from "remark-gfm";

import { useApp } from "../store";
import type { MessagePart, SessionMessageEntry } from "../../../shared/types";
import { toolChipContent } from "../../../shared/view";
import { InputPanel } from "./InputPanel";

/** Transcript — the joined doc entries, streaming-safe. Scroll stays pinned
 *  to the bottom while the user is already there. */
export function Transcript({ chatId }: { chatId: string }) {
  const transcript = useApp((s) => s.transcripts[chatId]);
  const scrollRef = useRef<HTMLDivElement>(null);
  const pinnedRef = useRef(true);
  const [, force] = useState(0);

  const version = transcript?.version ?? 0;

  useEffect(() => {
    if (pinnedRef.current && scrollRef.current) {
      scrollRef.current.scrollTop = scrollRef.current.scrollHeight;
    }
  }, [version]);

  // Re-render on every version bump (entries mutate in place).
  useEffect(() => force((n) => n + 1), [version]);

  const onScroll = () => {
    const el = scrollRef.current;
    if (!el) return;
    pinnedRef.current =
      el.scrollHeight - el.scrollTop - el.clientHeight < 80;
  };

  const entries = transcript?.entries ?? [];

  return (
    <div
      ref={scrollRef}
      onScroll={onScroll}
      className="min-h-0 flex-1 overflow-y-auto"
    >
      <div className="mx-auto max-w-[760px] px-4 py-6">
        {entries.length === 0 && (
          <div className="py-16 text-center text-[12.5px] text-ink-faint">
            Nothing here yet — send a message to start the session.
          </div>
        )}
        {entries.map((entry) => (
          <Entry key={entry.id} entry={entry} />
        ))}
      </div>
    </div>
  );
}

function Entry({ entry }: { entry: SessionMessageEntry }) {
  if (entry.role === "user") {
    return (
      <div className="mb-4 flex justify-end">
        <div className="max-w-[85%] rounded-xl border border-line bg-overlay px-3.5 py-2.5">
          {entry.parts.map((part) =>
            part.kind === "text" ? (
              <div
                key={part.id}
                className="whitespace-pre-wrap text-[13px] text-ink"
              >
                {part.text}
              </div>
            ) : null,
          )}
        </div>
      </div>
    );
  }

  const streaming = entry.status === "streaming";
  return (
    <div className="mb-5">
      {entry.parts.map((part) => (
        <Part key={part.id} part={part} />
      ))}
      {streaming && (
        <span className="streaming-dot mt-1 inline-block h-1.5 w-1.5 rounded-full bg-accent" />
      )}
      {entry.status === "aborted" && (
        <div className="mt-1 text-[11px] text-ink-faint">Interrupted</div>
      )}
    </div>
  );
}

function Part({ part }: { part: MessagePart }) {
  switch (part.kind) {
    case "text":
      return (
        <div className="md text-[13px] text-ink">
          <Markdown remarkPlugins={[remarkGfm]}>{part.text}</Markdown>
        </div>
      );
    case "reasoning":
      return <Reasoning text={part.text} />;
    case "tool":
      return <ToolRow part={part} />;
    case "input":
      return part.resolved ? null : (
        <InputPanel requestId={part.requestId} questions={part.questions} />
      );
    case "error":
      return (
        <div className="my-1 rounded-md border border-danger/40 bg-danger/10 px-3 py-2 text-[12px] text-danger">
          {part.message}
        </div>
      );
    default:
      return null;
  }
}

function Reasoning({ text }: { text: string }) {
  const [open, setOpen] = useState(false);
  return (
    <div className="my-1">
      <button
        onClick={() => setOpen(!open)}
        className="flex items-center gap-1.5 text-[11px] italic text-ink-faint hover:text-ink-muted"
      >
        <span
          className={`inline-block transition-transform ${open ? "rotate-90" : ""}`}
        >
          ›
        </span>
        Thinking
      </button>
      {open && (
        <div className="md mt-1 border-l-2 border-line pl-3 text-[12px] italic text-ink-muted">
          <Markdown remarkPlugins={[remarkGfm]}>{text}</Markdown>
        </div>
      )}
    </div>
  );
}

function ToolRow({ part }: { part: Extract<MessagePart, { kind: "tool" }> }) {
  const [open, setOpen] = useState(false);
  const [label, detail] = toolChipContent(part.call);
  const hasBody = Boolean(part.output || part.diffStats?.length);
  return (
    <div className="my-1">
      <button
        onClick={() => hasBody && setOpen(!open)}
        className={`flex max-w-full items-center gap-2 rounded-md border border-line bg-card px-2.5 py-1 text-left ${
          hasBody ? "hover:bg-hover" : "cursor-default"
        }`}
      >
        <Dot state={part.isError ? "error" : part.resolved ? "done" : "running"} />
        <span className="shrink-0 text-[11px] font-medium text-ink-muted">
          {label}
        </span>
        {detail && (
          <span className="truncate font-mono text-[11px] text-ink-faint">
            {detail}
          </span>
        )}
        {part.subagentStatus === "running" && part.subagentTail && (
          <span className="truncate text-[10.5px] italic text-ink-faint">
            {part.subagentTail}
          </span>
        )}
      </button>
      {open && part.output && (
        <pre className="mt-1 max-h-[320px] overflow-auto whitespace-pre-wrap rounded-md border border-line bg-surface p-2.5 font-mono text-[11px] text-ink-muted">
          {part.output}
        </pre>
      )}
      {open && part.diffStats && part.diffStats.length > 0 && (
        <div className="mt-1 flex flex-wrap gap-1.5">
          {part.diffStats.map((d) => (
            <span
              key={d.path}
              className="rounded border border-line bg-surface px-1.5 py-0.5 font-mono text-[10.5px] text-ink-faint"
            >
              {d.path}{" "}
              <span className="text-success">+{d.additions}</span>{" "}
              <span className="text-danger">−{d.deletions}</span>
            </span>
          ))}
        </div>
      )}
    </div>
  );
}

function Dot({ state }: { state: "running" | "done" | "error" }) {
  const cls =
    state === "running"
      ? "bg-accent streaming-dot"
      : state === "error"
        ? "bg-danger"
        : "bg-ink-faint";
  return <span className={`h-1.5 w-1.5 shrink-0 rounded-full ${cls}`} />;
}


