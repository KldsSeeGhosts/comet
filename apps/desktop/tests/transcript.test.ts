import { describe, expect, it } from "vitest";

import {
  applyTranscriptUpdate,
  TranscriptDesync,
  type TranscriptState,
} from "../src/shared/transcript";
import type { SessionMessageEntry } from "../src/shared/types";

function entry(id: string, text: string): SessionMessageEntry {
  return {
    id,
    role: "assistant",
    parts: [{ kind: "text", id: "t0", text }],
    createdAt: 0,
    deviceId: "dev",
  };
}

function state(entries: SessionMessageEntry[] = []): TranscriptState {
  return { entries, contextUsage: null };
}

describe("applyTranscriptUpdate", () => {
  it("applies a reset frame", () => {
    const s = state();
    applyTranscriptUpdate(s, { reset: [entry("a", "hi")] });
    expect(s.entries).toHaveLength(1);
    expect(s.entries[0].id).toBe("a");
  });

  it("applies upserts anchored on prior entries", () => {
    const s = state([entry("a", "1")]);
    applyTranscriptUpdate(s, {
      upsert: [{ after: "a", entry: entry("b", "2") }],
      append: [],
      remove: [],
      count: 2,
    });
    expect(s.entries.map((e) => e.id)).toEqual(["a", "b"]);
  });

  it("applies a streaming text append to a text part", () => {
    const s = state([entry("b", "wor")]);
    applyTranscriptUpdate(s, {
      upsert: [],
      append: [{ entry: "b", part: "t0", text: "ld", len: 5 }],
      remove: [],
      count: 1,
    });
    expect((s.entries[0].parts[0] as { text: string }).text).toBe("world");
  });

  it("applies appends to reasoning parts too", () => {
    const s = state([
      {
        id: "r",
        role: "assistant",
        parts: [{ kind: "reasoning", id: "r0", text: "think" }],
        createdAt: 0,
        deviceId: "dev",
      },
    ]);
    applyTranscriptUpdate(s, {
      upsert: [],
      append: [{ entry: "r", part: "r0", text: "ing", len: 8 }],
      remove: [],
      count: 1,
    });
    expect((s.entries[0].parts[0] as { text: string }).text).toBe("thinking");
  });

  it("removes entries and still checks the count", () => {
    const s = state([entry("a", "1"), entry("b", "2"), entry("c", "3")]);
    applyTranscriptUpdate(s, {
      upsert: [],
      append: [],
      remove: ["b"],
      count: 2,
    });
    expect(s.entries.map((e) => e.id)).toEqual(["a", "c"]);
  });

  it("repositions an upsert mid-list when its anchor moved", () => {
    const s = state([entry("a", "1"), entry("c", "3")]);
    applyTranscriptUpdate(s, {
      upsert: [{ after: "a", entry: entry("b", "2") }],
      append: [],
      remove: [],
      count: 3,
    });
    expect(s.entries.map((e) => e.id)).toEqual(["a", "b", "c"]);
  });

  it("throws TranscriptDesync on a missing anchor", () => {
    const s = state([]);
    expect(() =>
      applyTranscriptUpdate(s, {
        upsert: [{ after: "missing", entry: entry("x", "1") }],
        append: [],
        remove: [],
        count: 1,
      }),
    ).toThrow(TranscriptDesync);
  });

  it("throws TranscriptDesync on append length mismatch", () => {
    const s = state([entry("a", "hello")]);
    expect(() =>
      applyTranscriptUpdate(s, {
        upsert: [],
        append: [{ entry: "a", part: "t0", text: "x", len: 99 }],
        remove: [],
        count: 1,
      }),
    ).toThrow(TranscriptDesync);
  });

  it("throws TranscriptDesync on count mismatch", () => {
    const s = state([entry("a", "1")]);
    expect(() =>
      applyTranscriptUpdate(s, {
        upsert: [],
        append: [],
        remove: [],
        count: 5,
      }),
    ).toThrow(TranscriptDesync);
  });

  it("carries contextUsage across frames", () => {
    const s = state();
    applyTranscriptUpdate(s, {
      reset: [],
      contextUsage: { tokens: 12, window: 100, components: [] },
    });
    expect(s.contextUsage?.tokens).toBe(12);
    applyTranscriptUpdate(s, {
      upsert: [{ after: null, entry: entry("a", "1") }],
      append: [],
      remove: [],
      count: 1,
      contextUsage: { tokens: 40, window: 100, components: [] },
    });
    expect(s.contextUsage?.tokens).toBe(40);
  });
});
