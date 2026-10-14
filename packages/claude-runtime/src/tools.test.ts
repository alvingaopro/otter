// The pinned SDK's built-in tools are all in tools.json (D-066), which
// otterd's tests check are all classified: a tool the SDK adds fails here
// until it's listed, and there until policy knows what it is.

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { createRequire } from "node:module";
import { dirname, join } from "node:path";
import { test } from "node:test";

test("every built-in tool of the pinned SDK is listed for otterd's policy", () => {
  const require = createRequire(import.meta.url);
  const sdk = dirname(require.resolve("@anthropic-ai/claude-agent-sdk"));
  const types = readFileSync(join(sdk, "sdk-tools.d.ts"), "utf8");
  const union = /export type ToolInputSchemas =([^;]*);/.exec(types);
  assert.ok(union, "ToolInputSchemas not found in sdk-tools.d.ts");
  const members = (union[1] ?? "")
    .split("|")
    .map((m) => m.trim())
    .filter(Boolean);
  assert.ok(members.length > 30, `${members.length} tools`);
  const listed = JSON.parse(readFileSync(new URL("../tools.json", import.meta.url), "utf8")).tools as Record<string, string | null>;
  const missing = members.filter((m) => !(m in listed));
  assert.deepEqual(missing, [], "add these to tools.json (and classify them in claude_stream.rs::tool_call)");
});
