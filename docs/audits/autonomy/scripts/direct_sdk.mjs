// Mode A of the autonomy benchmark: the Claude Agent SDK called directly,
// with the options Otter's worker uses (packages/claude-runtime/src/sdk.ts
// `Session.options()`), minus Otter: no appended instructions, and a
// PreToolUse hook + canUseTool that allow everything (an all-allow policy),
// counting how often each is consulted.
//
//   node direct_sdk.mjs <sdk-dir> <cwd> <prompt> <out.json>
//
// <sdk-dir>: a node_modules containing @anthropic-ai/claude-agent-sdk (use the
// one the Otter worker ships, so the version is identical).
import { createRequire } from "node:module";
import { writeFileSync } from "node:fs";
import { pathToFileURL } from "node:url";

const [sdkDir, cwd, prompt, out] = process.argv.slice(2);
const require = createRequire(pathToFileURL(sdkDir + "/").href);
const sdk = await import(pathToFileURL(require.resolve("@anthropic-ai/claude-agent-sdk")).href);

const counts = { hook: 0, canUseTool: 0, tools: {}, results: [], text_blocks: 0 };
const t0 = Date.now();
let firstEdit = null;

async function* input() {
  yield {
    type: "user",
    message: { role: "user", content: prompt },
    parent_tool_use_id: null,
    session_id: "",
  };
}

const q = sdk.query({
  prompt: input(),
  options: {
    cwd,
    env: { ...process.env },
    settingSources: ["project"],
    systemPrompt: { type: "preset", preset: "claude_code" },
    permissionMode: "default",
    disallowedTools: ["Task", "Agent"],
    hooks: {
      PreToolUse: [{
        hooks: [async (i) => {
          counts.hook += 1;
          counts.tools[i.tool_name] = (counts.tools[i.tool_name] || 0) + 1;
          if (!firstEdit && /^(Edit|Write|MultiEdit)$/.test(i.tool_name)) firstEdit = (Date.now() - t0) / 1000;
          return { hookSpecificOutput: { hookEventName: "PreToolUse", permissionDecision: "allow" } };
        }],
      }],
    },
    canUseTool: async (_tool, input) => {
      counts.canUseTool += 1;
      return { behavior: "allow", updatedInput: input };
    },
  },
});

for await (const m of q) {
  if (m.type === "assistant") {
    for (const b of m.message?.content || []) if (b.type === "text") counts.text_blocks += 1;
  }
  if (m.type === "result") {
    const { subtype, is_error, num_turns, duration_ms, total_cost_usd, session_id, usage } = m;
    counts.results.push({ subtype, is_error, num_turns, duration_ms, total_cost_usd, session_id, usage });
  }
}
counts.wall_s = (Date.now() - t0) / 1000;
counts.time_to_first_edit_s = firstEdit;
writeFileSync(out, JSON.stringify(counts, null, 2));
console.log(JSON.stringify({ wall_s: counts.wall_s, hook: counts.hook, canUseTool: counts.canUseTool, results: counts.results.map((r) => ({ ...r, usage: undefined })) }));
