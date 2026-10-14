#!/usr/bin/env node
// A live check for Brain Runtime V2 (D-089, M0's spike): can the Agent SDK
// run Otter's own profile — none of Claude Code's tools, only tools the
// worker defines — under this host's Claude credentials? It uses real Claude
// and costs a little usage, so it is run by hand, never in CI:
//
//   node probes/otter-tools.mjs [model]
//
// It asks about a task through a single in-process tool, then checks: which
// tools the session offered, that only Otter's tool was called (and with what),
// that the answer used its result, which credentials were used, and that
// nothing outside the tool ran. It prints what it observed.

import { mkdtempSync, readdirSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createSdkMcpServer, query, tool } from "@anthropic-ai/claude-agent-sdk";
import { z } from "zod";

const model = process.argv[2] ?? "haiku";
const dir = mkdtempSync(join(tmpdir(), "otter-tools-"));
const results = [];
const check = (name, ok, detail = "") => {
  results.push({ name, ok });
  console.log(`${ok ? "PASS" : "FAIL"}  ${name}${detail ? ` — ${detail}` : ""}`);
};

const calls = [];
const otter = createSdkMcpServer({
  name: "otter",
  version: "0.0.0",
  tools: [
    tool(
      "get_task",
      "Where one of the developer's tasks stands: its status, and what it waits on.",
      { task_id: z.string() },
      async (args) => {
        calls.push(args);
        return {
          content: [
            {
              type: "text",
              text: JSON.stringify({
                ok: true,
                data: {
                  id: args.task_id,
                  status: "blocked",
                  waiting_on: "the developer: may the agent run `rm -rf build/`?",
                },
              }),
            },
          ],
        };
      },
    ),
  ],
});

let init;
let answer = "";
const used = [];
const q = query({
  prompt: "Why is task feat_42 blocked? Look it up with your tool, then answer in one sentence.",
  options: {
    cwd: dir,
    model,
    tools: [],
    mcpServers: { otter },
    strictMcpConfig: true,
    allowedTools: ["mcp__otter__get_task"],
    disallowedTools: ["Task", "Agent"],
    settingSources: [],
    permissionMode: "default",
    maxTurns: 4,
    systemPrompt: "You are Otter. You can only look things up with your tools.",
    // Anything not allowed above would end here: nothing should.
    canUseTool: async (name) => {
      used.push(`asked:${name}`);
      return { behavior: "deny", message: "not in Otter's profile" };
    },
  },
});
for await (const m of q) {
  if (m.type === "system" && m.subtype === "init") init = m;
  if (m.type === "assistant") {
    for (const b of m.message.content) {
      if (b.type === "tool_use") used.push(b.name);
      if (b.type === "text") answer += b.text;
    }
  }
  if (m.type === "result") {
    answer = m.result ?? answer;
    console.log(`(result: ${m.subtype}, turns ${m.num_turns}, cost $${m.total_cost_usd})`);
  }
}

const offered = init?.tools ?? [];
console.log(`offered tools: ${JSON.stringify(offered)}`);
console.log(`auth: ${init?.apiKeySource}; claude code ${init?.claude_code_version}; model ${init?.model}`);
check("only Otter's tool is offered", offered.length === 1 && offered[0] === "mcp__otter__get_task", JSON.stringify(offered));
check("Otter's tool was called with the task", calls.length === 1 && calls[0].task_id === "feat_42", JSON.stringify(calls));
check("nothing else was used or asked for", used.every((u) => u === "mcp__otter__get_task"), JSON.stringify(used));
check("the answer comes from the tool", /rm -rf build|rm\b/.test(answer), JSON.stringify(answer));
check("the working directory is untouched", readdirSync(dir).length === 0);
process.exit(results.every((r) => r.ok) ? 0 : 1);
