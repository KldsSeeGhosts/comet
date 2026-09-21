/**
 * Chunked blob storage over a DO's SQLite. Durable Object SQL caps individual
 * values at ~2MB; session snapshots and diff sidecars can exceed that, so
 * named blobs are stored as ordered chunk rows.
 */

export const CHUNK_BYTES = 1_500_000;

export interface BlobStore {
  put(name: string, bytes: Uint8Array): void;
  get(name: string): Uint8Array | undefined;
  /** Read only chunks intersecting [start, end), retaining the full length. */
  getRange(name: string, start: number, end?: number): { bytes: Uint8Array; total: number } | undefined;
  delete(name: string): void;
}

export const createBlobStore = (sql: SqlStorage): BlobStore => {
  sql.exec(
    "CREATE TABLE IF NOT EXISTS blobs (name TEXT NOT NULL, idx INTEGER NOT NULL, bytes BLOB NOT NULL, PRIMARY KEY (name, idx))"
  );
  return {
    put(name, bytes) {
      sql.exec("DELETE FROM blobs WHERE name = ?", name);
      for (let i = 0, idx = 0; i === 0 || i < bytes.length; i += CHUNK_BYTES, idx++) {
        const chunk = bytes.subarray(i, Math.min(i + CHUNK_BYTES, bytes.length));
        sql.exec("INSERT INTO blobs (name, idx, bytes) VALUES (?, ?, ?)", name, idx, chunk.buffer.slice(chunk.byteOffset, chunk.byteOffset + chunk.byteLength));
      }
    },
    get(name) {
      return this.getRange(name, 0)?.bytes;
    },
    getRange(name, start, end) {
      if (!Number.isSafeInteger(start) || start < 0 ||
          (end !== undefined && (!Number.isSafeInteger(end) || end < start))) {
        throw new RangeError("Invalid blob range");
      }
      const size = [...sql.exec("SELECT SUM(length(bytes)) AS total FROM blobs WHERE name = ?", name)][0]?.total;
      if (size === null || size === undefined) return undefined;
      const total = Number(size);
      const stop = Math.min(end ?? total, total);
      const out = new Uint8Array(Math.max(0, stop - start));
      if (out.length === 0) return { bytes: out, total };
      // Iterate the cursor directly: do not retain every chunk beside the output.
      for (const row of sql.exec(
        "SELECT idx, bytes FROM blobs WHERE name = ? AND idx >= ? AND idx <= ? ORDER BY idx",
        name, Math.floor(start / CHUNK_BYTES), Math.floor((stop - 1) / CHUNK_BYTES)
      )) {
        const bytes = new Uint8Array(row.bytes as ArrayBuffer);
        const offset = Number(row.idx) * CHUNK_BYTES;
        const lo = Math.max(start, offset);
        const hi = Math.min(stop, offset + bytes.length);
        out.set(bytes.subarray(lo - offset, hi - offset), lo - start);
      }
      return { bytes: out, total };
    },
    delete(name) {
      sql.exec("DELETE FROM blobs WHERE name = ?", name);
    }
  };
};

export const textEncoder = new TextEncoder();
export const textDecoder = new TextDecoder();

export const putJsonBlob = (store: BlobStore, name: string, value: unknown): void =>
  store.put(name, textEncoder.encode(JSON.stringify(value)));

export const getJsonBlob = <T>(store: BlobStore, name: string): T | undefined => {
  const bytes = store.get(name);
  if (!bytes) return undefined;
  try {
    return JSON.parse(textDecoder.decode(bytes)) as T;
  } catch {
    return undefined;
  }
};
