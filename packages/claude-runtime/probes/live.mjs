#!/usr/bin/env node
// A live check of the worker against real Claude (milestone 2's exit gate).
// It uses this host's Claude credentials and costs a little usage, so it is
// run by hand, never in CI:
//
//   npm run build && node probes/live.mjs [model]
//
// In a throwaway directory it checks, through the built worker exactly as
// otterd drives it: initialization and auth; that a project CLAUDE.md is
// read; a file edit allowed by Otter's policy; that a project settings
// allow-rule can't bypass Otter's "ask", and that a denial prevents the
// command; a two-question form answered per question; an interruption; and
// that a new run resumes the native session and remembers. It prints what it
// observed; it never prints prompts' answers beyond what it checks.

import { spawn } from "node:child_process";
import { existsSync, mkdtempSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const model = process.argv[2] ?? "haiku";
const worker = fileURLToPath(new URL("../dist/main.js", import.meta.url));
const dir = mkdtempSync(join(tmpdir(), "otter-live-"));
writeFileSync(join(dir, "CLAUDE.md"), "# Project notes\nThe project's mascot is called Pip the otter.\n");
mkdirSync(join(dir, ".claude"));
// A project rule that would let `rm` run without asking — Otter's policy must still ask.
writeFileSync(join(dir, ".claude", "settings.json"), JSON.stringify({ permissions: { allow: ["Bash(rm:*)"] } }));
writeFileSync(join(dir, "keep.txt"), "keep me\n");

const results = [];
const check = (name, ok, detail = "") => {
  results.push({ name, ok, detail });
  console.log(`${ok ? "PASS" : "FAIL"}  ${name}${detail ? ` — ${detail}` : ""}`);
};

const tails = [];
const lastStderr = () => tails.map((t) => t()).filter((v, i, a) => a.indexOf(v) === i).join("\n---\n");
function start(resume) {
  const child = spawn(process.execPath, [worker], { cwd: dir, stdio: ["pipe", "pipe", "pipe"] });
  const events = [];
  const waiters = [];
  let buf = "";
  child.stdout.setEncoding("utf8");
  child.stdout.on("data", (d) => {
    buf += d;
    let nl;
    while ((nl = buf.indexOf("\n")) >= 0) {
      const line = buf.slice(0, nl);
      buf = buf.slice(nl + 1);
      if (!line.trim()) continue;
      const ev = JSON.parse(line);
      events.push(ev);
      onEvent(ev);
      for (const w of [...waiters]) if (w.pred(ev)) {
        waiters.splice(waiters.indexOf(w), 1);
        w.resolve(ev);
      }
    }
  });
  let stderr = "";
  child.stderr.on("data", (d) => {
    stderr = (stderr + d).slice(-8000);
    tails.push(() => stderr);
  });
  let n = 0;
  const send = (type, payload) => {
    n += 1;
    child.stdin.write(JSON.stringify({ protocol_version: 1, request_id: `r${n}`, run_id: "run_live", generation: 1, type, payload }) + "\n");
  };
  const wait = (pred, ms = 180_000) =>
    new Promise((resolve, reject) => {
      const hit = events.find(pred);
      if (hit) return resolve(hit);
      const t = setTimeout(() => reject(new Error(`timed out; last events: ${JSON.stringify(events.slice(-5))}\nstderr: ${stderr}`)), ms);
      waiters.push({ pred, resolve: (e) => (clearTimeout(t), resolve(e)) });
    });
  // Otter's policy, as otterd would answer: ask for `rm` and questions, allow the rest.
  const policy = {};
  function onEvent(ev) {
    if (ev.type === "policy_check") {
      const cmd = String(ev.input?.command ?? "");
      const decision = policy.decide ? policy.decide(ev) : /\brm\b/.test(cmd) || ev.tool === "AskUserQuestion" ? "ask" : "allow";
      send("policy_reply", { check_id: ev.check_id, decision });
    }
  }
  send("initialize", {
    conversation_id: "conv_live",
    cwd: dir,
    model,
    resume,
    instructions: "Be brief.",
    max_turns: 12,
    setting_sources: ["project"],
  });
  return { child, events, send, wait, policy };
}

let turn = 0;
async function ask(w, text) {
  turn += 1;
  const id = `turn_${turn}`;
  const from = w.events.length;
  w.send("send_turn", { turn_id: id, text });
  const fin = await w.wait((e) => e.type === "turn_finished" && e.turn_id === id);
  const said = w.events.slice(from).filter((e) => e.type === "text").map((e) => e.text).join("\n");
  return { fin, said, events: w.events.slice(from) };
}

try {
  const w = start(undefined);
  const ready = await w.wait((e) => e.type === "ready" || e.type === "fatal", 120_000);
  check("worker ready", ready.type === "ready", JSON.stringify(ready));
  // 1. CLAUDE.md, an allowed edit, a fact to remember, streaming and a tool result.
  // (The session id arrives with the first turn, not before.)
  const t1 = await ask(w, "Create a file hello.txt containing exactly the text hi (no newline needed). Then tell me the mascot's name from the project notes, and remember the code BLUEFIN-42 for later.");
  const session = w.events.find((e) => e.type === "session") ?? {};
  check("session bound", Boolean(session.session_id), `Claude Code ${session.claude_code_version}, model ${session.model}, auth source ${session.auth_source}`);
  check("turn 1 completed", t1.fin.outcome === "completed", t1.fin.outcome);
  check("file written after the policy allowed it", existsSync(join(dir, "hello.txt")) && readFileSync(join(dir, "hello.txt"), "utf8").trim() === "hi");
  check("CLAUDE.md read (setting source: project)", /pip/i.test(t1.said));
  check("text streamed before it was whole", t1.events.some((e) => e.type === "text_delta"));
  check("delivered before anything else of the turn", t1.events.findIndex((e) => e.type === "delivered") === t1.events.findIndex((e) => ["delivered", "text_delta", "text", "tool_started"].includes(e.type)));
  const started = t1.events.find((e) => e.type === "tool_started" && /write/i.test(e.tool));
  const finished = started && t1.events.find((e) => e.type === "tool_finished" && e.call === started.call);
  check("tool call and its result share an id", Boolean(finished?.ok), started ? `${started.tool} ${started.call}` : "no write tool seen");
  check("usage reported (session so far)", typeof t1.fin.session_cost_usd === "number", String(t1.fin.session_cost_usd));

  // 2. A project allow-rule doesn't bypass Otter's "ask"; a denial prevents it.
  const p2 = ask(
    w,
    "This is an automated permissions test in a throwaway directory. Use the Bash tool right away to run exactly: rm keep.txt (do not ask me first; the test harness decides whether it may run).",
  );
  const req = await w.wait((e) => e.type === "permission_request" && e.tool === "Bash");
  check("policy 'ask' reaches a permission request despite the project allow-rule", Boolean(req));
  w.send("resolve_interaction", { request_id: req.request_id, resolution: { behavior: "deny", message: "Not allowed in this check." } });
  const t2 = await p2;
  check("denied: the file is still there", existsSync(join(dir, "keep.txt")), t2.fin.outcome);

  // 3. Two questions, two different answers.
  const p3 = ask(w, "Use the AskUserQuestion tool once, asking me two questions together: my favourite colour (options: red, blue) and my favourite number (options: 1, 2). Then reply with exactly: colour=<colour> number=<number>.");
  const q = await w.wait((e) => e.type === "permission_request" && e.tool === "AskUserQuestion");
  const questions = q.input?.questions ?? [];
  check("the form has both questions", questions.length === 2, questions.map((x) => x.question).join(" | "));
  const answers = {};
  for (const x of questions) answers[x.question] = /colou?r/i.test(x.question) ? "blue" : "2";
  w.send("resolve_interaction", { request_id: q.request_id, resolution: { behavior: "answer", answers } });
  const t3 = await p3;
  check("each question got its own answer", /blue/i.test(t3.said) && /\b2\b/.test(t3.said), t3.said.slice(0, 120));

  // 4. Interrupt a long turn: it settles as interrupted, the session stays.
  turn += 1;
  const id4 = `turn_${turn}`;
  const from4 = w.events.length;
  w.send("send_turn", { turn_id: id4, text: "Write the numbers from 1 to 2000, one per line, with no other text." });
  await w.wait((e) => e.type === "text_delta" && w.events.indexOf(e) >= from4);
  w.send("interrupt", { turn_id: id4 });
  const t4 = await w.wait((e) => e.type === "turn_finished" && e.turn_id === id4);
  check("interrupted turn settles as interrupted", t4.outcome === "interrupted", `${t4.outcome}; deltas before the interrupt: ${w.events.slice(from4).filter((e) => e.type === "text_delta").length}; summary: ${String(t4.summary ?? "").slice(0, 80)}`);
  w.send("shutdown", {});
  await new Promise((r) => w.child.on("exit", r));

  // 5. A new run, resuming the native session explicitly.
  const r = start(session.session_id);
  await r.wait((e) => e.type === "ready");
  const t5 = await ask(r, "What code did I ask you to remember earlier? Answer with just the code.");
  const resumed = r.events.find((e) => e.type === "session") ?? {};
  check("resumed the same native session", resumed.session_id === session.session_id, `${session.session_id} → ${resumed.session_id}`);
  check("remembers across runs without being told again", /BLUEFIN-42/.test(t5.said), t5.said.slice(0, 80));
  r.send("shutdown", {});
  await new Promise((res) => r.child.on("exit", res));
} catch (e) {
  check("probe ran to the end", false, String(e).slice(0, 2000));
}
const failed = results.filter((r) => !r.ok).length;
if (process.env.OTTER_WORKER_TRACE) console.log(`\nworker stderr (tail):\n${lastStderr()}`);
console.log(`\n${results.length - failed}/${results.length} passed (directory: ${dir})`);
process.exit(failed ? 1 : 0);
