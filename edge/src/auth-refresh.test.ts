import { afterEach, expect, it, vi } from "vitest";
import { handleAuthRoute } from "./auth-routes";
import type { Env } from "./env";

afterEach(() => vi.unstubAllGlobals());
const refresh = () => {
  const url = new URL("https://edge.example/auth/refresh");
  return handleAuthRoute(new Request(url, {
    method: "POST", headers: { "content-type": "application/json" },
    body: JSON.stringify({ refreshToken: "fixture-refresh", organizationId: "fixture-org" })
  }), { WORKOS_API_KEY: "fixture-key", WORKOS_CLIENT_ID: "fixture-client" } as Env, url);
};
it("preserves terminal OAuth refresh rejection for clients", async () => {
  vi.stubGlobal("fetch", vi.fn(async () => Response.json({ error: "invalid_grant", error_description: "Refresh revoked" }, { status: 400 })));
  const response = (await refresh())!;
  expect(response.status).toBe(401);
  expect(await response.json()).toEqual({ code: "invalid_grant", error: "Refresh revoked" });
});
it.each([401, 429, 503])("does not invalidate user credentials on upstream status %i", async (status) => {
  vi.stubGlobal("fetch", vi.fn(async () => Response.json({ error: "upstream_unavailable" }, { status })));
  const response = (await refresh())!;
  expect(response.status).toBe(502);
  expect(await response.json()).not.toHaveProperty("code");
});
it("reports network failure as retryable", async () => {
  vi.stubGlobal("fetch", vi.fn(async () => { throw new TypeError("network unavailable"); }));
  const response = (await refresh())!;
  expect(response.status).toBe(502);
  expect(await response.json()).not.toHaveProperty("code");
});
