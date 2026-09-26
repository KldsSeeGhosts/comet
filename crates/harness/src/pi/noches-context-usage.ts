// Noches context-usage reporter. pi-acp never emits ACP `usage_update`, so
// this extension writes Pi's own `ctx.getContextUsage()` snapshot to the file
// named by NOCHES_PI_USAGE_FILE; the harness polls that file and emits the
// same `ContextUsage` event the other agents reach through their adapters.
// Self-contained: it never touches the computer-use bridge and must keep
// working when CUA is disabled or unavailable. Never throw, never block.
import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";
import { renameSync, writeFileSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { homedir } from "node:os";

interface UsageSnapshot {
  tokens: number | null;
  contextWindow: number;
  percent: number | null;
  model?: string;
  /** Auto-compaction threshold percent (contextWindow - reserveTokens). */
  compactionPercent?: number;
  sessionTokens?: { input: number; output: number; cacheRead: number };
  ts: number;
}

interface TokenUsage {
  input?: number;
  output?: number;
  cacheRead?: number;
}

const DEFAULT_RESERVE_TOKENS = 16384;

// Pi merges global ~/.pi/agent/settings.json over the built-in defaults; a
// project .pi/settings.json can further override. Only the documented
// reserveTokens keys are read - anything else is ignored.
function configuredReserveTokens(cwd: string): number | undefined {
  const candidates = [
    join(homedir(), ".pi", "agent", "settings.json"),
    join(cwd, ".pi", "settings.json"),
  ];
  let reserve: number | undefined;
  for (const path of candidates) {
    try {
      const parsed = JSON.parse(readFileSync(path, "utf8")) as {
        compaction?: { reserveTokens?: unknown };
      };
      const value = parsed?.compaction?.reserveTokens;
      if (typeof value === "number" && Number.isSafeInteger(value) && value >= 0) {
        reserve = value;
      }
    } catch {
      // A missing or malformed settings file leaves the previous layer.
    }
  }
  return reserve;
}

// The same threshold the compaction trigger uses: contextTokens >
// contextWindow - reserveTokens. Model-specific overrides resolve through
// pi's settings manager, which extensions cannot reach; with the ordinary
// reserve this is exact, otherwise the field stays absent rather than lying.
function compactionPercent(
  contextWindow: number,
  reserveTokens: number | undefined,
): number | undefined {
  if (contextWindow <= 0 || reserveTokens === undefined) return undefined;
  const percent = ((contextWindow - reserveTokens) / contextWindow) * 100;
  return percent > 0 && percent < 100 ? percent : undefined;
}

// Mirrors AgentSession.getSessionStats(): dedicated usage entries plus the
// usage of compaction and branch-summary entries and ordinary messages.
function sessionTokenTotals(ctx: ExtensionContext): UsageSnapshot["sessionTokens"] {
  try {
    let input = 0;
    let output = 0;
    let cacheRead = 0;
    let seen = false;
    const add = (u: TokenUsage | undefined) => {
      if (!u) return;
      input += typeof u.input === "number" ? u.input : 0;
      output += typeof u.output === "number" ? u.output : 0;
      cacheRead += typeof u.cacheRead === "number" ? u.cacheRead : 0;
      seen = true;
    };
    for (const entry of ctx.sessionManager.getEntries()) {
      const e = entry as {
        type?: string;
        usage?: TokenUsage;
        message?: { role?: string; usage?: TokenUsage };
      };
      if (e.type === "usage") {
        add(e.usage);
      } else if (e.type === "compaction" || e.type === "branch_summary") {
        add(e.usage);
      } else if (e.type === "message") {
        add(e.message?.usage);
      }
    }
    return seen ? { input, output, cacheRead } : undefined;
  } catch {
    return undefined;
  }
}

function snapshot(ctx: ExtensionContext): UsageSnapshot | undefined {
  const usage = ctx.getContextUsage();
  if (!usage || typeof usage.contextWindow !== "number" || usage.contextWindow <= 0) {
    return undefined;
  }
  const tokens = typeof usage.tokens === "number" ? usage.tokens : null;
  const percent = typeof usage.percent === "number" ? usage.percent : null;
  const reserve = configuredReserveTokens(ctx.cwd) ?? DEFAULT_RESERVE_TOKENS;
  return {
    tokens,
    contextWindow: usage.contextWindow,
    percent,
    model: ctx.model ? `${ctx.model.provider}/${ctx.model.id}` : undefined,
    compactionPercent: compactionPercent(usage.contextWindow, reserve),
    sessionTokens: sessionTokenTotals(ctx),
    ts: Date.now(),
  };
}

export default function (pi: ExtensionAPI) {
  const target = process.env.NOCHES_PI_USAGE_FILE;
  if (!target) return;
  const tmp = `${target}.tmp`;

  const write = (ctx: ExtensionContext) => {
    try {
      const body = snapshot(ctx);
      if (!body) return;
      writeFileSync(tmp, JSON.stringify(body), { mode: 0o600 });
      renameSync(tmp, target);
    } catch {
      // A failed write must never interrupt the agent; the next event retries.
    }
  };

  pi.on("session_start", (_event, ctx) => write(ctx));
  pi.on("message_end", (event, ctx) => {
    if ((event.message as { role?: string }).role === "assistant") write(ctx);
  });
  pi.on("turn_end", (_event, ctx) => write(ctx));
  pi.on("agent_end", (_event, ctx) => write(ctx));
  // Right after compaction Pi reports tokens/percent as null until the next
  // assistant response; writing that null snapshot is what lets the ring
  // show "waiting" instead of a stale pre-compaction number.
  pi.on("session_compact", (_event, ctx) => write(ctx));
  pi.on("session_compact_failed", (_event, ctx) => write(ctx));
}
