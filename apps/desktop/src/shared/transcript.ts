//! Consumer side of `WatchDocMessages` — a TypeScript port of
//! `crates/doc/src/transcript_delta.rs::apply_transcript_frame`.
//!
//! A frame is either `{reset: entries}` (full snapshot) or a delta
//! `{upsert, append, remove, count}`. Any apply failure means the local copy
//! has diverged; the caller must resubscribe for a fresh reset.

import type {
  ContextUsage,
  SessionMessageEntry,
  TranscriptUpdate,
} from "./types";

export class TranscriptDesync extends Error {
  constructor(message: string) {
    super(`transcript delta desync: ${message}`);
    this.name = "TranscriptDesync";
  }
}

export interface TranscriptState {
  entries: SessionMessageEntry[];
  contextUsage: ContextUsage | null;
}

/** Apply one stream item in place. Throws TranscriptDesync on divergence. */
export function applyTranscriptUpdate(
  state: TranscriptState,
  update: TranscriptUpdate,
): void {
  if (update.contextUsage !== undefined) {
    state.contextUsage = update.contextUsage;
  }
  if ("reset" in update) {
    state.entries = update.reset;
    return;
  }

  const { upsert, append, remove, count } = update;
  const current = state.entries;

  if (remove.length > 0) {
    const gone = new Set(remove);
    for (let i = current.length - 1; i >= 0; i--) {
      if (gone.has(current[i].id)) current.splice(i, 1);
    }
  }

  for (const u of upsert) {
    const existing = current.findIndex((e) => e.id === u.entry.id);
    if (existing >= 0) current.splice(existing, 1);
    let at = 0;
    if (u.after !== null) {
      const anchor = current.findIndex((e) => e.id === u.after);
      if (anchor < 0) {
        throw new TranscriptDesync(`missing anchor ${u.after}`);
      }
      at = anchor + 1;
    }
    current.splice(at, 0, u.entry);
  }

  for (const a of append) {
    const target = current.find((e) => e.id === a.entry);
    if (!target) {
      throw new TranscriptDesync(`missing append entry ${a.entry}`);
    }
    const part = target.parts.find(
      (p): p is { kind: "text" | "reasoning"; id: string; text: string } =>
        (p.kind === "text" || p.kind === "reasoning") && p.id === a.part,
    );
    if (!part) {
      throw new TranscriptDesync(`missing append part ${a.part}`);
    }
    part.text += a.text;
    if (part.text.length !== a.len) {
      throw new TranscriptDesync(
        `append length mismatch on ${a.entry}#${a.part}: ` +
          `have ${part.text.length}, expected ${a.len}`,
      );
    }
  }

  if (current.length !== count) {
    throw new TranscriptDesync(
      `count mismatch: have ${current.length}, expected ${count}`,
    );
  }
}
