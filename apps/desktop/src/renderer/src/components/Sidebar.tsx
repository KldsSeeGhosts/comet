import { useMemo } from "react";

import { useApp } from "../store";
import type { Chat, Space } from "../../../shared/types";
import { spaceDisplayName } from "../../../shared/types";
import {
  chatLocation,
  displayStatus,
  formatTimeAgo,
  sortChats,
  sortSpaces,
} from "../../../shared/view";

export function Sidebar() {
  const spaces = useApp((s) => s.spaces);
  const chats = useApp((s) => s.chats);
  const sessions = useApp((s) => s.sessions);
  const selectedChatId = useApp((s) => s.selectedChatId);
  const selectChat = useApp((s) => s.selectChat);
  const newChat = useApp((s) => s.newChat);
  const now = useApp((s) => s.now);
  const conn = useApp((s) => s.conn);
  const engineInfo = useApp((s) => s.engineInfo);
  const connectivity = useApp((s) => s.connectivity);

  const grouped = useMemo(() => {
    const bySpace = new Map<string, Chat[]>();
    const unassigned: Chat[] = [];
    for (const chat of sortChats(chats)) {
      if (chat.archived) continue;
      if (chat.spaceId) {
        const list = bySpace.get(chat.spaceId) ?? [];
        list.push(chat);
        bySpace.set(chat.spaceId, list);
      } else {
        unassigned.push(chat);
      }
    }
    return { spaces: sortSpaces(spaces), bySpace, unassigned };
  }, [spaces, chats]);

  return (
    <aside className="flex w-[248px] shrink-0 flex-col border-r border-line bg-surface">
      <div className="flex h-11 items-center gap-2 border-b border-line px-3">
        <div className="h-4 w-4 rounded bg-accent-strong" />
        <div className="text-[13px] font-semibold tracking-tight">Noches</div>
        <div className="ml-auto text-[10px] uppercase tracking-wider text-ink-faint">
          dev
        </div>
      </div>

      <div className="min-h-0 flex-1 overflow-y-auto px-2 py-2">
        {grouped.spaces.map((space) => (
          <SpaceSection
            key={space.id}
            space={space}
            chats={grouped.bySpace.get(space.id) ?? []}
            sessions={sessions}
            selectedChatId={selectedChatId}
            now={now}
            onSelect={selectChat}
            onNew={() => void newChat(space.id)}
          />
        ))}
        {grouped.unassigned.length > 0 && (
          <div className="mt-3">
            <div className="px-2 pb-1 text-[10.5px] font-medium uppercase tracking-wider text-ink-faint">
              No project
            </div>
            {grouped.unassigned.map((chat) => (
              <ChatRow
                key={chat.id}
                chat={chat}
                status={displayStatus(chat, sessions[chat.id], new Date(now))}
                selected={chat.id === selectedChatId}
                now={now}
                onSelect={selectChat}
              />
            ))}
          </div>
        )}
        {grouped.spaces.length === 0 && grouped.unassigned.length === 0 && (
          <div className="px-2 py-6 text-center text-[12px] text-ink-faint">
            No sessions yet.
          </div>
        )}
      </div>

      <button
        onClick={() => void newChat(undefined)}
        className="mx-2 mb-2 rounded-md border border-line bg-card px-3 py-1.5 text-left text-[12px] text-ink-muted hover:bg-hover hover:text-ink"
      >
        + New session
      </button>

      <div className="border-t border-line px-3 py-2 text-[10.5px] text-ink-faint">
        <div className="flex items-center gap-1.5">
          <StatusDot conn={conn.kind} connectivity={connectivity?.state} />
          <span className="truncate">
            {conn.kind === "ready"
              ? `engine · ${engineInfo?.deviceId?.slice(0, 8) ?? "?"}`
              : "engine offline"}
          </span>
          <span className="ml-auto font-mono">27655</span>
        </div>
      </div>
    </aside>
  );
}

function StatusDot({ conn, connectivity }: { conn: string; connectivity?: string }) {
  const color =
    conn !== "ready"
      ? "bg-danger"
      : connectivity === "online"
        ? "bg-success"
        : "bg-ink-faint";
  return <span className={`h-1.5 w-1.5 rounded-full ${color}`} />;
}

function SpaceSection({
  space,
  chats,
  sessions,
  selectedChatId,
  now,
  onSelect,
  onNew,
}: {
  space: Space;
  chats: Chat[];
  sessions: Record<string, import("../../../shared/types").Session>;
  selectedChatId: string | null;
  now: number;
  onSelect: (id: string) => void;
  onNew: () => void;
}) {
  return (
    <div className="mb-1">
      <div className="group flex items-center gap-1 px-2 pb-1 pt-2">
        <div className="min-w-0 flex-1">
          <div className="truncate text-[11.5px] font-medium text-ink-muted">
            {spaceDisplayName(space)}
          </div>
          <div className="truncate font-mono text-[9.5px] text-ink-faint">
            {space.path}
          </div>
        </div>
        <button
          onClick={onNew}
          title="New session in this space"
          className="rounded px-1 text-[13px] text-ink-faint opacity-0 hover:bg-hover hover:text-ink group-hover:opacity-100"
        >
          +
        </button>
      </div>
      {chats.map((chat) => (
        <ChatRow
          key={chat.id}
          chat={chat}
          status={displayStatus(chat, sessions[chat.id], new Date(now))}
          selected={chat.id === selectedChatId}
          now={now}
          onSelect={onSelect}
        />
      ))}
      {chats.length === 0 && (
        <div className="px-2 py-1 text-[11px] text-ink-faint">No sessions</div>
      )}
    </div>
  );
}

function ChatRow({
  chat,
  status,
  selected,
  now,
  onSelect,
}: {
  chat: Chat;
  status: ReturnType<typeof displayStatus>;
  selected: boolean;
  now: number;
  onSelect: (id: string) => void;
}) {
  const title = chat.title?.trim() || chat.lastMessagePreview?.trim() || "Untitled";
  const sub = chatLocation(chat);
  const when = formatTimeAgo(
    new Date(chat.lastMessageAt ?? chat.createdAt),
    new Date(now),
  );
  return (
    <button
      onClick={() => onSelect(chat.id)}
      className={`flex w-full items-start gap-2 rounded-md px-2 py-1.5 text-left ${
        selected ? "bg-active" : "hover:bg-hover"
      }`}
    >
      <IndicatorDot status={status} />
      <div className="min-w-0 flex-1">
        <div
          className={`truncate text-[12.5px] ${
            status === "completed" ? "font-medium text-ink" : "text-ink-muted"
          } ${selected ? "text-ink" : ""}`}
        >
          {title}
        </div>
        <div className="flex items-baseline gap-1.5">
          {sub && (
            <div className="truncate text-[10.5px] text-ink-faint">{sub}</div>
          )}
          <div className="ml-auto shrink-0 text-[10px] text-ink-faint">
            {when}
          </div>
        </div>
      </div>
    </button>
  );
}

function IndicatorDot({ status }: { status: ReturnType<typeof displayStatus> }) {
  const cls =
    status === "working"
      ? "bg-accent streaming-dot"
      : status === "awaitingInput"
        ? "bg-warning"
        : status === "errored"
          ? "bg-danger"
          : status === "completed"
            ? "bg-ink-faint"
            : "bg-transparent";
  return <span className={`mt-[6px] h-1.5 w-1.5 shrink-0 rounded-full ${cls}`} />;
}
