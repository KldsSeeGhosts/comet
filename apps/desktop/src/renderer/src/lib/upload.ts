//! Chunked attachment upload + transcript read-back over the RPC bridge —
//! the TypeScript port of `crates/ui/src/attachments.rs`'s
//! `upload_attachment` / `read_attachment_image`. Bytes travel as base64
//! `UploadChunk{uploadId,seq,data}` frames (positional seq = idempotent
//! retry), then `UploadCommit{uploadId,fileName}` returns the durable
//! absolute path on the target device.

import { methods } from "../../../shared/protocol";
import {
  CHUNK_TIMEOUT_MS,
  COMMIT_TIMEOUT_MS,
  chunkRanges,
  FIRST_CHUNK_TIMEOUT_MS,
  attachmentDeadlineMs,
  MAX_READ_CHUNKS,
  READ_CHUNK_TIMEOUT_MS,
  UPLOAD_CONCURRENCY,
} from "../../../shared/attachments";
import * as rpc from "./rpc";

function withTarget(
  params: Record<string, unknown>,
  targetDeviceId?: string,
): Record<string, unknown> {
  if (targetDeviceId) params.targetDeviceId = targetDeviceId;
  return params;
}

/** Race an RPC against `timeoutMs` — a stalled-but-open relay link never
 *  fails a call on its own, so every attachment call races a timer. */
function callWithTimeout<T>(
  method: string,
  params: unknown,
  timeoutMs: number,
): Promise<T> {
  return new Promise<T>((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error(`${method} timed out`)), timeoutMs);
    rpc
      .call<T>(method, params)
      .then((v) => {
        clearTimeout(timer);
        resolve(v);
      })
      .catch((err) => {
        clearTimeout(timer);
        reject(err instanceof Error ? err : new Error(String(err)));
      });
  });
}

export interface UploadTarget {
  /** Relay-forward target — the chat's host device when it differs from the
   *  engine we're connected to. Omit for a local commit. */
  targetDeviceId?: string;
  uploadId: string;
  fileName: string;
  /** Whole-file base64. */
  base64: string;
  /** Accumulates uploaded BINARY bytes (the strip's percent reads it). */
  onProgress?: (doneBinaryBytes: number) => void;
}

/** Chunked upload → the durable absolute path on the target device. */
export async function uploadAttachment(t: UploadTarget): Promise<string> {
  const ranges = chunkRanges(t.base64.length);
  const deadline = Date.now() + attachmentDeadlineMs(ranges.length);

  // 3-wide chunk window; seq slots are idempotent engine-side so completion
  // order doesn't matter and a blind retry is safe.
  let next = 0;
  const worker = async () => {
    for (;;) {
      const i = next++;
      if (i >= ranges.length) return;
      const [seq, start, end] = ranges[i];
      const params = withTarget(
        { uploadId: t.uploadId, seq, data: t.base64.slice(start, end) },
        t.targetDeviceId,
      );
      // The first WINDOW gets the cold-dial allowance — its chunks all start
      // before the link is warm.
      const timeout =
        seq < UPLOAD_CONCURRENCY ? FIRST_CHUNK_TIMEOUT_MS : CHUNK_TIMEOUT_MS;
      let attempt = 0;
      for (;;) {
        if (Date.now() > deadline) {
          throw new Error(
            `attachment upload exceeded ${attachmentDeadlineMs(ranges.length) / 1000}s`,
          );
        }
        try {
          await callWithTimeout(methods.UploadChunk, params, timeout);
          break;
        } catch (err) {
          if (attempt >= 2) throw err;
          attempt += 1;
          // Stagger by seq so parallel chunks that failed together don't
          // re-collide in lockstep.
          await new Promise((r) => setTimeout(r, 50 * attempt * (seq + 1)));
        }
      }
      // b64 → binary bytes (final chunk's padding rounds up by ≤2 bytes —
      // irrelevant for a percentage).
      t.onProgress?.(Math.floor(((end - start) * 3) / 4));
    }
  };
  await Promise.all(
    Array.from({ length: Math.min(UPLOAD_CONCURRENCY, ranges.length) }, worker),
  );

  const reply = await callWithTimeout<{ path?: string }>(
    methods.UploadCommit,
    withTarget({ uploadId: t.uploadId, fileName: t.fileName }, t.targetDeviceId),
    COMMIT_TIMEOUT_MS,
  );
  if (typeof reply?.path !== "string" || reply.path.length === 0) {
    throw new Error("upload commit returned no path");
  }
  return reply.path;
}

export interface LoadedAttachment {
  name: string;
  mimeType: string;
  /** Whole-file base64 (45KB binary chunks concatenated). */
  base64: string;
}

interface AttachmentChunk {
  name?: string;
  mimeType?: string;
  data?: string;
  nextOffset?: number;
  done?: boolean;
}

/** `ReadAttachmentChunk` loop: 45KB base64 chunks until `done` (bounded,
 *  with the same stuck-offset guard as zeron's `readAttachmentImage`). */
export async function readAttachmentImage(
  path: string,
  targetDeviceId?: string,
): Promise<LoadedAttachment | null> {
  let name = "";
  let mimeType = "";
  let b64 = "";
  let offset = 0;
  let done = false;
  for (let i = 0; i < MAX_READ_CHUNKS; i++) {
    let chunk: AttachmentChunk;
    try {
      chunk = await callWithTimeout<AttachmentChunk>(
        methods.ReadAttachmentChunk,
        withTarget({ path, offset }, targetDeviceId),
        READ_CHUNK_TIMEOUT_MS,
      );
    } catch {
      return null;
    }
    if (
      typeof chunk?.name !== "string" ||
      typeof chunk.mimeType !== "string" ||
      typeof chunk.data !== "string" ||
      typeof chunk.done !== "boolean"
    ) {
      return null;
    }
    name = chunk.name;
    mimeType = chunk.mimeType;
    b64 += chunk.data;
    done = chunk.done;
    if (done) break;
    const next = chunk.nextOffset;
    if (typeof next !== "number" || next <= offset) return null;
    offset = next;
  }
  if (!done || b64.length === 0) return null;
  return {
    name: name.length > 0 ? name : "image",
    mimeType: mimeType || "image/png",
    base64: b64,
  };
}
