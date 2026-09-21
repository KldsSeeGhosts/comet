import { env, runInDurableObject } from "cloudflare:test";
import { expect, it } from "vitest";
import { appendRow, logStats } from "../../src/chat-log";
import { AUTH_USER_HEADER } from "../../src/env";
import { MAX_CHECKPOINT_BYTES } from "../../src/chat-room";

it("accepts a 21 MiB checkpoint through the production route and advances the floor", async () => {
  const stub = env.CHAT_ROOMS.get(env.CHAT_ROOMS.idFromName("whale-checkpoint"));
  await runInDurableObject(stub, (_instance, state) => {
    appendRow(state.storage.sql, "host", "row-1", new Uint8Array([1]), 0);
  });
  const bytes = new Uint8Array(21 * 1024 * 1024).fill(37);
  const response = await stub.fetch("https://chat/checkpoint?seqCovered=1", {
    method: "POST",
    headers: { [AUTH_USER_HEADER]: "user", "x-chat2-frontier": "AQ==" },
    body: bytes
  });
  expect(response.status).toBe(200);
  await response.arrayBuffer();
  const stats = await runInDurableObject(stub, (_instance, state) => logStats(state.storage.sql));
  expect(stats.checkpointSeq).toBe(1);
  expect(stats.checkpointSize).toBe(bytes.length);
  expect(stats.rowCount).toBe(0);
  const resumed = await stub.fetch("https://chat/checkpoint", {
    headers: { [AUTH_USER_HEADER]: "user", range: `bytes=${bytes.length - 1024}-` }
  });
  expect(resumed.status).toBe(206);
  expect(new Uint8Array(await resumed.arrayBuffer())).toEqual(new Uint8Array(1024).fill(37));
});

it("retains a finite checkpoint upload cap", async () => {
  const stub = env.CHAT_ROOMS.get(env.CHAT_ROOMS.idFromName("oversize-checkpoint"));
  const response = await stub.fetch("https://chat/checkpoint?seqCovered=0", {
    method: "POST", headers: { [AUTH_USER_HEADER]: "user" },
    body: new Uint8Array(MAX_CHECKPOINT_BYTES + 1)
  });
  expect(response.status).toBe(413);
  await response.arrayBuffer();
});


it("resumes across chunk boundaries and rejects ranges beyond the checkpoint", async () => {
  const { CHUNK_BYTES } = await import("../../src/blobs");
  const stub = env.CHAT_ROOMS.get(env.CHAT_ROOMS.idFromName("range-boundaries"));
  const bytes = Uint8Array.from({ length: CHUNK_BYTES * 2 + 17 }, (_, i) => i % 251);
  const uploaded = await stub.fetch("https://chat/checkpoint?seqCovered=0", {
    method: "POST", headers: { [AUTH_USER_HEADER]: "user" }, body: bytes
  });
  expect(uploaded.status).toBe(200);
  await uploaded.arrayBuffer();
  for (const start of [0, CHUNK_BYTES - 1, CHUNK_BYTES, CHUNK_BYTES + 1, bytes.length - 1]) {
    const response = await stub.fetch("https://chat/checkpoint", {
      headers: { [AUTH_USER_HEADER]: "user", range: `bytes=${start}-` }
    });
    expect(response.status).toBe(start === 0 ? 200 : 206);
    expect(response.headers.get("content-range")).toBe(start === 0 ? null : `bytes ${start}-${bytes.length - 1}/${bytes.length}`);
    const actual = new Uint8Array(await response.arrayBuffer());
    expect(actual.length).toBe(bytes.length - start);
    expect(actual.every((byte, i) => byte === bytes[start + i])).toBe(true);
  }
  for (const start of [bytes.length, bytes.length + 1]) {
    const response = await stub.fetch("https://chat/checkpoint", {
      headers: { [AUTH_USER_HEADER]: "user", range: `bytes=${start}-` }
    });
    expect(response.status).toBe(416);
    expect(response.headers.get("content-range")).toBe(`bytes */${bytes.length}`);
    await response.arrayBuffer();
  }
});
