import { describe, expect, it } from "vitest";

import type { Chat, Session } from "../src/shared/types";
import {
  chatLocation,
  displayStatus,
  effectiveIndicator,
  formatTimeAgo,
  groupChats,
  pendingInputRequest,
  projectLabel,
  sendButtonMode,
  SESSION_STALE_MS,
  sortChats,
  toolChipContent,
  toolGroupSummary,
} from "../src/shared/view";
import type { SessionMessageEntry } from "../src/shared/types";

function chat(partial: Partial<Chat>): Chat {
  return {
    id: "c",
    deviceId: "d",
    title: null,
    archived: false,
    cwd: null,
    branch: null,
    checkoutId: null,
    config: null,
    lastMessagePreview: null,
    lastMessageAt: null,
    createdAt: "2026-01-01T00:00:00Z",
    ...partial,
  };
}

function session(partial: Partial<Session>): Session {
  return {
    chatId: "c",
    deviceId: "d",
    status: "working",
    startedAt: null,
    updatedAt: new Date().toISOString(),
    ...partial,
  };
}

describe("effectiveIndicator", () => {
  it("gates working sessions on the 45s staleness window", () => {
    const now = new Date();
    const fresh = session({
      status: "working",
      updatedAt: new Date(now.getTime() - 1000).toISOString(),
    });
    expect(effectiveIndicator(fresh, now)).toBe("working");
    const stale = session({
      status: "working",
      updatedAt: new Date(now.getTime() - SESSION_STALE_MS - 1).toISOString(),
    });
    expect(effectiveIndicator(stale, now)).toBe("none");
  });

  it("idle and missing sessions show nothing", () => {
    expect(effectiveIndicator(undefined, new Date())).toBe("none");
    expect(
      effectiveIndicator(session({ status: "idle" }), new Date()),
    ).toBe("none");
  });
});

describe("displayStatus", () => {
  it("errored + unseen reads errored; errored + seen reads idle", () => {
    const now = new Date();
    const live = session({ status: "errored", updatedAt: now.toISOString() });
    const unseen = chat({ lastMessageAt: now.toISOString() });
    expect(displayStatus(unseen, live, now)).toBe("errored");
    const seen = chat({
      lastMessageAt: "2026-01-01T00:00:00Z",
      lastSeenAt: "2026-01-02T00:00:00Z",
    });
    expect(displayStatus(seen, live, now)).toBe("idle");
  });
});

describe("sortChats", () => {
  it("orders by lastMessageAt desc with createdAt fallback", () => {
    const a = chat({ id: "a", createdAt: "2026-01-01T00:00:00Z" });
    const b = chat({
      id: "b",
      createdAt: "2026-01-02T00:00:00Z",
      lastMessageAt: "2026-01-05T00:00:00Z",
    });
    const c = chat({ id: "c", createdAt: "2026-01-04T00:00:00Z" });
    expect(sortChats([a, b, c]).map((x) => x.id)).toEqual(["b", "c", "a"]);
  });
});

describe("grouping + labels", () => {
  it("projectLabel uses the cwd basename, with ~ as No project", () => {
    expect(projectLabel("/home/u/repo")).toBe("repo");
    expect(projectLabel("~")).toBe("No project");
    expect(projectLabel(null)).toBe("No project");
    expect(projectLabel("/home/u/repo/")).toBe("repo");
  });

  it("chatLocation joins project and branch", () => {
    expect(
      chatLocation(chat({ cwd: "/home/u/repo", branch: "dev" })),
    ).toBe("repo · dev");
    expect(chatLocation(chat({ cwd: "/home/u/repo" }))).toBe("repo");
    expect(chatLocation(chat({}))).toBeNull();
  });

  it("groupChats preserves recency order inside groups", () => {
    const a = chat({ id: "a", cwd: "/x/one" });
    const b = chat({ id: "b", cwd: "/x/two" });
    const c = chat({ id: "c", cwd: "/x/one" });
    const groups = groupChats([a, b, c]);
    expect(groups.map((g) => g.label)).toEqual(["one", "two"]);
    expect(groups[0].chats.map((x) => x.id)).toEqual(["a", "c"]);
  });
});

describe("formatTimeAgo", () => {
  const now = new Date("2026-06-01T12:00:00Z");
  it("formats the ladder", () => {
    expect(formatTimeAgo(new Date("2026-06-01T11:59:30Z"), now)).toBe("now");
    expect(formatTimeAgo(new Date("2026-06-01T11:55:00Z"), now)).toBe("5m");
    expect(formatTimeAgo(new Date("2026-06-01T09:00:00Z"), now)).toBe("3h");
    expect(formatTimeAgo(new Date("2026-05-30T12:00:00Z"), now)).toBe("2d");
    expect(formatTimeAgo(new Date("2026-05-18T12:00:00Z"), now)).toBe("2w");
  });
});

describe("toolChipContent", () => {
  it("names tools the same as the GPUI viewport", () => {
    expect(toolChipContent({ kind: "exec", command: "ls\n-la" })).toEqual([
      "Run",
      "ls -la",
    ]);
    expect(
      toolChipContent({ kind: "unknown", name: "Agent: scan repo" }),
    ).toEqual(["Agent", "scan repo"]);
    expect(toolChipContent({ kind: "unknown", name: "noches_cua", input: { action: "type_text" } })).toEqual([
      "Computer use",
      "type text",
    ]);
  });
});

describe("toolGroupSummary", () => {
  it("summarizes like zeron", () => {
    expect(
      toolGroupSummary([
        [{ kind: "exec", command: "a" }, false],
        [{ kind: "exec", command: "b" }, false],
        [{ kind: "editFile", path: "/x" }, false],
        [{ kind: "readFile", path: "/y" }, true],
      ]),
    ).toBe("Ran 2 commands · edited 1 file · read 1 file · 1 failed");
  });
});

describe("sendButtonMode", () => {
  it("idle sends, live+text queues, live+empty stops", () => {
    expect(sendButtonMode(false, false)).toBe("send");
    expect(sendButtonMode(false, true)).toBe("send");
    expect(sendButtonMode(true, true)).toBe("queue");
    expect(sendButtonMode(true, false)).toBe("stop");
  });
});

describe("pendingInputRequest", () => {
  const q = {
    id: "q1",
    header: "Pick",
    question: "Which?",
    options: ["a", "b"],
    multiSelect: false,
  };
  const inputEntry = (id: string, resolved: boolean): SessionMessageEntry => ({
    id,
    role: "assistant",
    parts: [{ kind: "input", id: "i1", requestId: `req-${id}`, questions: [q], resolved }],
    createdAt: 0,
    deviceId: "d",
  });

  it("finds the unresolved input on the last assistant entry", () => {
    const t = [inputEntry("e1", false)];
    expect(pendingInputRequest(t)?.requestId).toBe("req-e1");
  });

  it("a newer assistant entry supersedes an unanswered older question", () => {
    const t = [
      inputEntry("e1", false),
      {
        id: "e2",
        role: "assistant",
        parts: [{ kind: "text", id: "t", text: "done" }],
        createdAt: 1,
        deviceId: "d",
      } as SessionMessageEntry,
    ];
    expect(pendingInputRequest(t)).toBeNull();
  });

  it("a steer user entry after the question does not hide it", () => {
    const t = [
      inputEntry("e1", false),
      {
        id: "u1",
        role: "user",
        parts: [{ kind: "text", id: "t", text: "steer" }],
        createdAt: 1,
        deviceId: "d",
      } as SessionMessageEntry,
    ];
    expect(pendingInputRequest(t)?.requestId).toBe("req-e1");
  });
});
