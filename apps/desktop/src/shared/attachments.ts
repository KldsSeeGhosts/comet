//! Attachments — the TypeScript port of `crates/ui/src/attachments.rs`
//! (itself a port of zeron's `composer/use-attachments.ts` and
//! `control/message-attachments.ts`). Attachment refs ride the prompt as
//! plain text lines; the same text persists as the user doc entry, so both
//! viewports must parse it identically.

/// use-attachments.ts `MAX_ATTACHMENT_BYTES`.
export const MAX_ATTACHMENT_BYTES = 24 * 1024 * 1024;

/// Base64 chars per `UploadChunk`, sized against the relay's hard ceiling
/// (1 MiB WS frame; ~510 KB binary per chunk). Multiple of 4 so a slice of
/// the whole-file base64 stays independently decodable.
export const UPLOAD_CHUNK_B64_CHARS = 680_000;

/// Bounds the `ReadAttachmentChunk` read-back loop (state.ts
/// `MAX_ATTACHMENT_READ_CHUNKS`).
export const MAX_READ_CHUNKS = 1_000;

/// The body used for image-only sends.
export const ATTACHMENT_ONLY_TEXT = "See the attached image(s).";

/// Queued-attachment ref scheme: bytes named by identity,
/// `pending://{uploadId}/{fileName}` (crates/engine `uploads.rs`).
export const PENDING_REF_PREFIX = "pending://";

export function isPendingRef(path: string): boolean {
  return path.startsWith(PENDING_REF_PREFIX);
}

export function pendingRef(uploadId: string, fileName: string): string {
  return `${PENDING_REF_PREFIX}${uploadId}/${fileName}`;
}

// ---------------------------------------------------------------------------
// Text transport (message-attachments.ts)
// ---------------------------------------------------------------------------

/** How attachments ride the prompt (use-attachments.ts `withAttachments`):
 *  plain local paths appended to the text — the files are staged on the
 *  device that runs the agent, so the agent can open them with its own
 *  tools; the same text is what persists as the user doc entry. */
export function withAttachments(text: string, paths: string[]): string {
  if (paths.length === 0) return text;
  const refs = paths.map((p) => `- ${p}`);
  const body = text.length === 0 ? ATTACHMENT_ONLY_TEXT : text;
  return `${body}\n\nAttached images (local files — open them to view):\n${refs.join("\n")}`;
}

export interface UserImageAttachment {
  id: string;
  path: string;
  name: string;
}

export interface ParsedUserMessage {
  /** The visible prompt (refs trailer stripped; empty for image-only sends). */
  text: string;
  attachments: UserImageAttachment[];
}

function nameFromPath(path: string): string {
  const name = path.split(/[/\\]/).pop()?.trim() ?? "";
  return name.length > 0 ? name : "image";
}

/** Find the refs trailer: a blank line, then a line starting
 *  (case-insensitive) with `Attached images (local files` and ending `):`.
 *  Returns `[bodyEnd, refsStart]` — the tolerant equivalent of zeron's
 *  `ATTACHED_IMAGES_RE`. */
function findRefsMarker(content: string): [number, number] | null {
  const lower = content.toLowerCase();
  const needle = "\n\nattached images (local files";
  let from = 0;
  for (;;) {
    const rel = lower.indexOf(needle, from);
    if (rel < 0) return null;
    const gap = rel;
    const lineStart = gap + 2;
    const nl = content.indexOf("\n", lineStart);
    const lineEnd = nl < 0 ? content.length : nl;
    const line = content.slice(lineStart, lineEnd).replace(/\r$/, "");
    if (line.endsWith("):")) {
      return [gap, Math.min(lineEnd + 1, content.length)];
    }
    from = lineStart;
  }
}

/** `parseUserMessageImages`: split the visible prompt from its
 *  attachment-ref trailer. */
export function parseUserMessageImages(content: string): ParsedUserMessage {
  const marker = findRefsMarker(content);
  if (!marker) return { text: content, attachments: [] };
  const [bodyEnd, refsStart] = marker;
  const body = content.slice(0, bodyEnd).replace(/\s+$/, "");
  const attachments = content
    .slice(refsStart)
    .split("\n")
    .map((line) => line.trimStart())
    .filter((line) => line.startsWith("- "))
    .map((line) => line.slice(2).trim())
    .filter((path) => path.length > 0)
    .map((path, index) => ({
      id: `${index}:${path}`,
      name: nameFromPath(path),
      path,
    }));
  if (attachments.length === 0) {
    return { text: content, attachments };
  }
  return {
    text: body.trim() === ATTACHMENT_ONLY_TEXT ? "" : body,
    attachments,
  };
}

/** `userMessageRailText`: what the rail/sidebar shows for a user message. */
export function userMessageRailText(content: string): string {
  const parsed = parseUserMessageImages(content);
  if (parsed.text.trim().length > 0) return parsed.text;
  switch (parsed.attachments.length) {
    case 0:
      return content;
    case 1:
      return "Attached image";
    default:
      return `${parsed.attachments.length} attached images`;
  }
}

// ---------------------------------------------------------------------------
// Staging (use-attachments.ts intake)
// ---------------------------------------------------------------------------

/** Image types the whole pipeline supports: the intersection of the
 *  renderer's decoders and the engine's `mime_by_ext` read-back jail. */
export function mimeByExtension(fileName: string): string | null {
  const ext = fileName.split(".").pop()?.toLowerCase();
  switch (ext) {
    case "png":
      return "image/png";
    case "jpg":
    case "jpeg":
      return "image/jpeg";
    case "gif":
      return "image/gif";
    case "webp":
      return "image/webp";
    case "svg":
      return "image/svg+xml";
    case "bmp":
      return "image/bmp";
    case "tif":
    case "tiff":
      return "image/tiff";
    case "avif":
      return "image/avif";
    case "heic":
      return "image/heic";
    default:
      return null;
  }
}

function extForMime(mime: string): string {
  switch (mime) {
    case "image/jpeg":
      return "jpg";
    case "image/gif":
      return "gif";
    case "image/webp":
      return "webp";
    case "image/svg+xml":
      return "svg";
    case "image/bmp":
      return "bmp";
    case "image/tiff":
      return "tiff";
    case "image/avif":
      return "avif";
    case "image/heic":
      return "heic";
    default:
      return "png";
  }
}

/** `ensureExtension`: pasted screenshots often arrive as a bare "image" —
 *  make sure the staged name carries a type-matching extension. */
export function ensureExtension(name: string, mime: string): string {
  const parts = name.split(".");
  const ext = parts.length > 1 ? parts[parts.length - 1] : "";
  const hasExt =
    parts.length > 1 &&
    parts[0].length > 0 &&
    ext.length >= 2 &&
    ext.length <= 5 &&
    /^[a-zA-Z0-9]+$/.test(ext);
  return hasExt ? name : `${name}.${extForMime(mime)}`;
}

// ---------------------------------------------------------------------------
// Chunk planning + deadlines (attachments.rs)
// ---------------------------------------------------------------------------

/** The `(seq, start, end)` plan for a file's base64 chunks. An empty file
 *  still sends one empty chunk (the commit needs the uploadId staged). */
export function chunkRanges(b64Len: number): [number, number, number][] {
  const ranges: [number, number, number][] = [];
  let start = 0;
  let seq = 0;
  for (;;) {
    const end = Math.min(start + UPLOAD_CHUNK_B64_CHARS, b64Len);
    ranges.push([seq, start, end]);
    start = end;
    seq += 1;
    if (start >= b64Len) break;
  }
  return ranges;
}

/** Whole-attachment deadline: 120s + 15s/chunk, capped at 900s. */
export function attachmentDeadlineMs(nChunks: number): number {
  return Math.min(120_000 + 15_000 * nChunks, 900_000);
}

/// Per-call deadlines (desktop state.ts).
export const FIRST_CHUNK_TIMEOUT_MS = 90_000;
export const CHUNK_TIMEOUT_MS = 30_000;
export const COMMIT_TIMEOUT_MS = 150_000;
export const READ_CHUNK_TIMEOUT_MS = 20_000;
export const UPLOAD_CONCURRENCY = 3;

/** Transcript read-back retry ladder: 2s doubling, capped at 15s
 *  (`retry_delay`). */
export function retryDelayMs(attempts: number): number {
  return Math.min(2_000 << Math.min(attempts, 3), 15_000);
}
