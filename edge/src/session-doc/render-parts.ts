import type { MessagePart, ToolCall, ToolPartDiff } from "./control-types";

/**
 * Tool calls as stored in a session document.
 *
 * The transcript exposes expandable invocation cards, so the stored shape must
 * retain every argument supplied by the harness. Harness adapters own payload
 * limits before calls reach this layer.
 */
export type RenderToolCall = ToolCall;

/** A message part as stored in the session document. */
export type SessionMessagePart =
  | Exclude<MessagePart, { kind: "tool" }>
  | {
      readonly kind: "tool";
      readonly id: string;
      readonly call: RenderToolCall;
      readonly isError?: boolean;
      readonly output?: string;
      readonly diff?: ToolPartDiff;
    };

/** Preserve the complete invocation used by the transcript's detail card. */
export const sanitizeToolCall = (call: ToolCall | RenderToolCall): RenderToolCall => call;

/** Normalize tool parts before writing them to the session document. */
export const toRenderParts = (
  parts: ReadonlyArray<MessagePart | SessionMessagePart>
): ReadonlyArray<SessionMessagePart> =>
  parts.map((p) =>
    p.kind === "tool"
      ? {
          kind: "tool",
          id: p.id,
          call: sanitizeToolCall(p.call),
          ...(typeof p.isError === "boolean" ? { isError: p.isError } : {}),
          ...(typeof p.output === "string" ? { output: p.output } : {}),
          ...(p.diff !== undefined ? { diff: p.diff } : {})
        }
      : p
  );
