import { useApp } from "./store";
import { Gate } from "./components/Gate";
import { Sidebar } from "./components/Sidebar";
import { ChatView } from "./components/ChatView";
import { Composer } from "./components/Composer";
import { QueuePanel } from "./components/QueuePanel";

export default function App() {
  const conn = useApp((s) => s.conn);
  const selectedChatId = useApp((s) => s.selectedChatId);

  if (conn.kind !== "ready") {
    return <Gate conn={conn} />;
  }

  return (
    <div className="flex h-full bg-bg text-ink">
      <Sidebar />
      <main className="flex min-w-0 flex-1 flex-col">
        {selectedChatId ? (
          <>
            <ChatView chatId={selectedChatId} />
            <QueuePanel chatId={selectedChatId} />
            <Composer chatId={selectedChatId} />
          </>
        ) : (
          <EmptyCanvas />
        )}
      </main>
    </div>
  );
}

function EmptyCanvas() {
  return (
    <div className="flex flex-1 items-center justify-center">
      <div className="text-center">
        <div className="text-[15px] font-medium text-ink">Noches</div>
        <div className="mt-1 text-[12.5px] text-ink-faint">
          Pick a session in the sidebar, or start a new one.
        </div>
      </div>
    </div>
  );
}
