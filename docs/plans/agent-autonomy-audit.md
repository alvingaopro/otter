Otter Agent Autonomy Audit

> **Revision 2 (2026-10-13) — supersedes the original plan where they differ.**
>
> Architectural clarification from the requester: Otter's primary structured
> coding backend is the official **Claude Agent SDK** (`sdk`, D-057), which runs
> the bundled Claude Code engine inside `packages/claude-runtime`. Otter uses
> the SDK on purpose, to capture structured tool events, permission checks,
> approval requests and session state. **Replacing the SDK with the Claude Code
> CLI is out of scope.** The SDK integration, structured events, approval
> interception and session persistence are preserved; no production code is
> changed during the audit.
>
> Changes to the plan:
>
> 1. **Central question:** does Otter's Controller (and the runtime around the
>    SDK) interfere with the native autonomy of the Claude Agent SDK? Do not
>    assume the Controller is the root cause; validate every hypothesis with
>    source evidence and runtime measurements.
> 2. **SDK run and turn lifecycle** (new, in Phase 2): runs per feature, turns
>    per run, Controller-induced interruptions, session-resume frequency,
>    unnecessary prompt injection, worker start-up overhead.
> 3. **Permission pipeline** (Phase 3) is audited end to end as
>    `PreToolUse → ToolCheck → Otter policy → canUseTool → DecisionNeeded`:
>    redundant checks, added latency, excess user interaction.
> 4. **New deliverable `09-sdk-integration.md`:** a dedicated audit of
>    `packages/claude-runtime/` and `crates/daemon/src/agents/claude_sdk.rs`.
> 5. **Benchmark (Phase 6) modes are replaced by:**
>    - **A. Direct Claude Agent SDK** — the same SDK package and version the
>      worker ships, called directly with equivalent options.
>    - **B. Otter SDK runtime without the Controller** — the `sdk` backend with
>      the deterministic engine and no model calls (`OTTER_CONTROLLER=rules`;
>      `off` starts no runs at all, and no protocol request starts a run outside
>      a feature, so `rules` is the nearest available mode).
>    - **C. Otter SDK runtime with the Controller** — the `sdk` backend with the
>      developer's configured Control Agent model.
>
>    Identical SDK version, model, tasks, environment, and equivalent
>    permission policies wherever possible; differences that can't be removed
>    are recorded.
> 6. **Metrics:** completion rate, execution duration, SDK runs and turns,
>    Controller invocations, permission checks, actual human approval
>    requests, agent interruptions, repeated failures, recovery success rate.
>    Unavailable metrics are reported as unavailable.
> 7. Execution overhead is reported separately from UI noise.
>
> Evidence produced on the `legacy_cli` backend (the real dogfooding trace,
> which ran on that backend because it is the configured default) is kept, but
> labelled as such, and conclusions about the SDK path rest on SDK evidence.


Type: Architecture Audit + Runtime Investigation + Benchmark
Repository: alvingaopro/otter
Priority: P0
Mode: Investigation First — No Production Code Changes

1. Objective

Investigate why Otter’s managed coding-agent experience currently feels significantly less autonomous and productive than using Claude Code or Codex directly.

Observed symptoms:

1. Too many low-value progress messages.
2. Too many approvals and user decisions.
3. Slow task completion.
4. Repeated tool execution and timeout loops.
5. Coding agents frequently stop or wait instead of independently recovering.
6. The Control Agent appears to spend substantial effort supervising routine implementation details.
7. The user must actively manage the agent rather than simply delegate a task and receive a verified result.

The primary hypothesis is:

Otter’s orchestration layer may be interfering with the native autonomy of its underlying coding agents.

This hypothesis must be tested, not assumed.

The investigation should determine whether the problems originate from:

- Otter’s Controller and Brain.
- The managed agent runtime.
- Approval and permission policies.
- Conversation actor and message routing.
- Agent lifecycle and execution management.
- Timeout and recovery mechanisms.
- UI event presentation.
- The underlying coding-agent provider.
- Development environment issues.
- Interactions between multiple components.

1. Investigation Principles

Follow these rules throughout the audit.

Rule 1: Evidence before conclusions

Do not assume the Controller is responsible.

Every major finding must include concrete evidence such as source-code references, runtime traces, tests, or reproducible behavior.

Rule 2: Separate execution problems from presentation problems

A noisy UI does not necessarily mean the agent is executing inefficiently.

A slow agent does not necessarily mean the Controller is interfering.

Investigate these independently.

Rule 3: Preserve the existing architecture during investigation

Do not refactor production code during the initial audit.

Temporary instrumentation is permitted only if isolated and reversible.

Rule 4: Compare against native agent execution

Whenever possible, compare Otter-managed execution against the same task executed directly using Claude Code.

Use identical:

- Model and provider configuration.
- Repository state.
- Task instructions.
- Environment.
- Tool permissions.
- Execution budget.

Document differences that cannot be eliminated.

Rule 5: Prioritize user experience

The goal is not to maximize architectural sophistication.

The goal is to minimize human intervention while reliably delivering completed development tasks.

⸻

Phase 1 — Architecture and Execution Flow Audit

1.1 Map the complete execution pipeline

Inspect the following components:

crates/daemon/src/controller.rs
crates/daemon/src/brain.rs
crates/daemon/src/runs.rs
crates/daemon/src/runtime/
crates/daemon/src/attention.rs
crates/daemon/src/reconcile.rs
crates/daemon/src/features.rs
crates/daemon/src/agents/
packages/claude-runtime/
apps/desktop/

Also inspect:

docs/design.md
docs/decisions.md
docs/invariants.md
docs/protocol.md
AGENTS.md

Do not rely exclusively on architectural documentation.

Verify that the implementation matches the documented design.

1.2 Reconstruct the actual control flow

Trace the lifecycle of a feature from creation to completion.

At minimum, identify:

User submits feature
|
v
Feature created
|
v
Controller activated
|
v
Brain evaluates feature
|
v
Coding agent launched
|
v
Agent executes tools
|
v
Tool results processed
|
v
Agent continues / stops / waits
|
v
Controller evaluates progress
|
v
Feature completes / fails / requests attention

For every transition, determine:

1. Which component owns the decision?
2. Is the decision deterministic or model-driven?
3. Does it require an LLM call?
4. Does it restart or interrupt the coding agent?
5. Does it introduce additional latency?
6. Can it block progress?
7. Does it require user input?
8. What state is persisted?
9. How does execution resume after a restart?

Produce a sequence diagram reflecting the actual implementation.

1.3 Identify unnecessary orchestration

Investigate whether the Controller is making decisions that the underlying coding agent could already handle.

Examples:

- Deciding whether to run tests.
- Deciding whether to inspect another file.
- Deciding whether to retry a failed command.
- Deciding whether to fix a compilation error.
- Deciding whether to continue after a routine tool call.
- Replanning implementation details unnecessarily.

Classify each Controller intervention:

Category Description
Essential Required for lifecycle, safety, or user intent
Useful Adds measurable value without significant overhead
Redundant Duplicates native agent capabilities
Harmful Interrupts execution or reduces autonomy
Unknown Requires runtime evidence

For each intervention, explain why it belongs in that category.

Deliverable

Create:

docs/audits/autonomy/01-execution-architecture.md

Include:

- Actual execution sequence diagram.
- Component responsibility matrix.
- Controller intervention inventory.
- Potential autonomy bottlenecks.
- Source-code references.

⸻

Phase 2 — Controller and Brain Investigation

2.1 Determine Controller execution frequency

Investigate:

- When does the Controller wake up?
- What events trigger Brain calls?
- How frequently does it invoke the model?
- Does every agent message trigger Controller evaluation?
- Does every tool result trigger Controller evaluation?
- Does the Controller wait for agent execution to finish?
- Can it interrupt an active agent?
- Can it launch redundant agent runs?

Measure or instrument:

controller_invocations
brain_model_calls
agent_run_starts
agent_run_interruptions
agent_run_restarts
controller_wait_duration
controller_decision_latency

2.2 Investigate agent continuity

Determine whether the coding agent retains uninterrupted control over its task.

Specifically:

- Does the underlying Claude session remain alive?
- Is the agent restarted between tasks?
- Does each Controller decision create a new agent turn?
- Are tool calls executed inside one continuous agent session?
- Does Otter inject additional prompts?
- Can injected prompts disrupt the agent’s current plan?
- Is conversation context preserved correctly?

Distinguish:

Session continuity: The same provider conversation is preserved.

Execution continuity: The agent can continue working without orchestration-induced pauses.

These are not equivalent.

A system may preserve conversation history while repeatedly interrupting execution.

2.3 Examine Controller prompts

Inspect every prompt used by the Brain and Controller.

Look for instructions that encourage:

- Excessive progress reporting.
- Frequent requests for permission.
- Repeated planning.
- Unnecessary confirmation.
- Stopping after intermediate milestones.
- Asking the user questions that can be answered from repository context.
- Delegating decisions back to the Controller.

Evaluate whether prompts encourage the agent to:

Execute until the task is complete or genuinely blocked.

Or instead:

Execute one step, report progress, and wait.

Deliverable

Create:

docs/audits/autonomy/02-controller-analysis.md

Include a call graph and evidence-backed assessment of whether Controller intervention reduces native agent autonomy.

⸻

Phase 3 — Approval and Permission Audit

3.1 Trace the approval pipeline

Investigate the complete lifecycle:

Agent requests tool
|
v
Permission evaluation
|
+-- Automatically approved
|
+-- Automatically denied
|
+-- Requires user approval

Determine:

- Which component classifies risk?
- Which permissions are inherited from Claude Code?
- Which permissions are controlled by Otter?
- Are there duplicate approval systems?
- Can an approval be cached?
- Are equivalent operations repeatedly evaluated?
- Are automatically approved operations still surfaced as user-facing decisions?
- Can a low-risk permission request pause execution unnecessarily?

  3.2 Examine the observed approval behavior

The screenshot shows numerous low-risk permission records.

Examples include:

Read: read

And commands such as:

ls -la ~/Downloads

Determine whether these records are:

1. Purely informational.
2. Blocking execution.
3. Triggering Controller calls.
4. Persisted as approval objects.
5. Counted as user attention.
6. Displayed because the UI does not distinguish auto-approved events from pending decisions.

These are separate potential problems.

Do not assume an approval record means the agent was blocked.

3.3 Evaluate permission policy

Assess whether Otter can safely support three permission levels:

Level Behavior
Automatic Execute within established sandbox boundaries
Conditional Execute when explicitly allowed by workspace policy
Interactive Require user approval for sensitive operations

Pay particular attention to:

- Workspace boundaries.
- External filesystem access.
- Network access.
- Credentials.
- Destructive operations.
- Git operations.
- Production infrastructure.

Do not recommend unrestricted execution merely to improve speed.

Deliverable

Create:

docs/audits/autonomy/03-approval-analysis.md

Include an approval decision matrix and identify unnecessary user interruptions.

⸻

Phase 4 — Timeout, Recovery, and Loop Investigation

The screenshot shows repeated 30-minute timeouts and repeated test execution.

This requires special attention.

4.1 Identify all timeout mechanisms

Find every timeout associated with:

- Controller calls.
- Brain calls.
- Coding-agent runs.
- Tool execution.
- Shell commands.
- Test execution.
- Provider API calls.
- Session reconciliation.

For each timeout, document:

Owner
Default duration
Configuration
Trigger
Cancellation behavior
Process cleanup
State transition
Recovery behavior

4.2 Investigate timeout propagation

Determine what happens when an agent run times out.

Questions:

- Is the underlying process terminated?
- Are child processes terminated?
- Can a test process continue running?
- Does Otter distinguish agent timeout from tool timeout?
- Does the Controller receive a structured error?
- Can the agent inspect partial output?
- Is the next run aware of the previous timeout?
- Can the same command be repeated indefinitely?

  4.3 Investigate recovery behavior

Evaluate these scenarios:

Scenario A: Compilation failure

The agent should inspect the error and attempt a fix.

Scenario B: Test failure

The agent should identify the failing test and investigate.

Scenario C: Test execution hangs

The runtime should provide enough information to distinguish a slow test from a stuck process.

Scenario D: Missing dependency

The agent should identify the missing dependency and either recover within its permissions or report a genuine blocker.

Scenario E: Repeated identical failure

The system should recognize the lack of progress and avoid unbounded repetition.

Scenario F: Agent process crashes

Otter should preserve state and provide a recoverable execution path.

4.4 Define recovery ownership

Determine which recovery operations belong to:

- Coding Agent.
- Runtime.
- Controller.
- User.

Avoid building a second coding agent inside the Controller.

Deliverable

Create:

docs/audits/autonomy/04-recovery-analysis.md

Include a failure-state diagram and a recovery responsibility matrix.

⸻

Phase 5 — Conversation and Attention Audit

5.1 Classify user-visible messages

Inspect the events and messages emitted by:

- Coding Agent.
- Controller.
- Brain.
- Runtime.
- Attention Manager.
- Desktop UI.

Classify them into:

Type Example
User decision Choose between incompatible product requirements
Important progress Implementation complete
Recoverable issue One test failed; investigating
Execution detail Running cargo test
Debug event Tool invocation parameters
Internal reasoning summary Controller planning next step
Final result Tests passed, commit created

Determine which categories appear in the main conversation.

5.2 Investigate attention semantics

Inspect attention.rs and related state transitions.

Determine whether Otter correctly distinguishes:

Working
Needs user
Blocked
Completed
Failed

Pay particular attention to whether:

- Auto-approved permissions generate attention.
- Agent uncertainty generates unnecessary attention.
- Routine errors generate attention before recovery is attempted.
- Informational messages are treated as decisions.
- Resolved approvals remain visible as pending work.

  5.3 Investigate feature creation with missing information

The screenshot shows a question similar to:

The feature request for Otter came through empty. What should this feature do?

Investigate:

- How empty feature requests are accepted.
- Whether validation occurs before execution.
- Whether the Controller attempts to infer missing requirements.
- Whether a missing requirement can be resolved from existing context.
- Why the question appeared after execution had already started.

Distinguish missing essential requirements from ordinary implementation ambiguity.

The system should not guess critical user intent.

However, it should not request confirmation for routine implementation choices.

Deliverable

Create:

docs/audits/autonomy/05-conversation-attention.md

Include:

- Message taxonomy.
- Attention-state analysis.
- Examples of unnecessary user-visible events.
- Recommendations for separating execution logs from conversation.

⸻

Phase 6 — Native Claude Code vs Otter Benchmark

This is the most important validation phase.

6.1 Benchmark objective

Measure whether Otter-managed execution performs worse than native Claude Code on identical development tasks.

Do not rely on subjective impressions alone.

6.2 Benchmark modes

Run the same tasks in three modes where supported:

Mode A — Native Claude Code

User
|
v
Claude Code
|
v
Tools

Mode B — Otter managed runtime

User
|
v
Otter managed run
|
v
Claude Code
|
v
Tools

Mode C — Otter full feature orchestration

User
|
v
Otter Controller
|
v
Managed run
|
v
Claude Code
|
v
Tools

The three modes are essential.

Comparing only native Claude Code with full Otter orchestration cannot reveal whether overhead comes from the managed runtime or the Controller.

If Mode B is not directly accessible, document the limitation. Do not modify production architecture solely to make the benchmark possible.

6.3 Benchmark tasks

Use representative tasks from the Otter repository.

Select at least five tasks:

Task Category
Fix a deterministic failing unit test Bug fixing
Implement a small isolated feature Feature development
Refactor a module without changing behavior Refactoring
Diagnose a reproducible build failure Environment/debugging
Implement a feature requiring multiple files Multi-step development

Tasks must have objective completion criteria.

Avoid vague requests such as:

Improve the code.

Prefer:

Fix this specific failing test without changing its expected behavior.

For each task, establish a baseline repository revision and an independent verification procedure.

Use separate worktrees or disposable repository copies to prevent contamination between runs.

6.4 Metrics

Collect:

Metric Description
Completion rate Tasks meeting acceptance criteria
Wall-clock duration Start to verified completion
Time to first edit Time before meaningful code modification
Model calls Total provider requests
Token usage Input and output tokens, when available
Controller calls Number of orchestration decisions
Agent restarts Number of interrupted/restarted runs
Tool calls Total tool executions
Human interventions Number of required user actions
Approval requests Automatic versus interactive
Repeated commands Duplicate or near-identical commands
Timeout count Number of timeouts
Recovery success Failures resolved without human intervention
Verification result Independent acceptance-test outcome

Report unavailable metrics as unavailable.

Do not estimate missing values.

6.5 Benchmark fairness

Control for:

- Model version.
- Model configuration.
- Task prompt.
- Repository revision.
- Dependency availability.
- Warm versus cold caches.
- Network conditions.
- Permission configuration.
- Maximum execution budget.

Use the same security boundaries wherever possible.

Record differences that cannot be controlled.

Run each task multiple times when cost and time permit, because agent performance is nondeterministic.

A single run is useful for diagnosis but insufficient for strong performance conclusions.

6.6 Benchmark execution safety

Do not run destructive experiments against the primary development workspace.

Use disposable worktrees or isolated environments.

Do not push benchmark commits.

Do not modify production credentials.

Do not run unlimited retries.

Use bounded time and cost budgets.

If a task cannot be executed safely, document the limitation and provide a reproducible manual procedure.

Deliverable

Create:

docs/audits/autonomy/06-benchmark-results.md

Include raw measurements, comparisons, and reproducibility instructions.

⸻

Phase 7 — Root Cause Analysis

After completing the previous phases, produce a consolidated diagnosis.

7.1 Rank findings

For each issue, provide:

Issue:
Observed symptom:
Root cause:
Evidence:
Affected components:
Impact:
Confidence:
Recommended fix:
Estimated complexity:

Use the following confidence levels:

- Confirmed: Reproduced or directly demonstrated.
- Strong evidence: Supported by multiple independent observations.
- Hypothesis: Plausible but not yet validated.

  7.2 Separate root causes from symptoms

Examples:

Symptom: Too many messages.

Possible root causes:

- Runtime events are displayed as chat messages.
- Controller generates excessive progress updates.
- Agent prompts encourage verbose reporting.

Symptom: Slow development.

Possible root causes:

- Excessive Controller model calls.
- Agent execution interruptions.
- Long-running tests.
- Environment problems.
- Repeated recovery attempts.

Symptom: Too many approvals.

Possible root causes:

- Overly restrictive policy.
- Duplicate permission systems.
- Poor approval caching.
- UI displaying already-approved operations.

Do not merge unrelated causes into one architectural conclusion.

7.3 Evaluate the central hypothesis

Explicitly answer:

Is Otter’s orchestration layer reducing the native autonomy of Claude Code?

Possible conclusions:

1. Yes, significantly.
2. Yes, but only in specific scenarios.
3. No, the primary problems originate elsewhere.
4. Insufficient evidence.

Support the conclusion with implementation evidence and benchmark results.

⸻

Phase 8 — Proposed Architecture and Remediation Roadmap

Only after the investigation is complete, propose changes.

Do not implement them during this audit.

Evaluate whether Otter should move toward:

User
|
v
Task Manager
|
+---- Attention Manager
|
+---- Task Supervisor
|
v
Coding Agent
|
v
Native Agent Runtime
|
v
Tools / Repository

The proposed architecture should preserve Otter’s core product requirements:

- Multiple remote hosts.
- Persistent workspaces.
- Persistent agent sessions.
- SSH-based connectivity.
- Multiple coding-agent providers.
- Reliable event persistence.
- Safe execution boundaries.
- User intervention when genuinely necessary.
- Verified task completion.

Avoid introducing unnecessary abstraction layers.

Prefer simplifying existing components over adding new subsystems.

Required roadmap

Produce prioritized recommendations.

P0 — Autonomy and execution reliability

Address issues directly preventing task completion.

P1 — Approval and recovery

Reduce unnecessary human intervention and improve failure handling.

P2 — Conversation and progress presentation

Separate user-facing updates from execution events.

P3 — Advanced supervision

Introduce additional monitoring only where benchmarks demonstrate a need.

For every recommendation, include:

- Expected user benefit.
- Supporting evidence.
- Affected files.
- Implementation complexity.
- Regression risks.
- Acceptance criteria.
- How improvement will be measured.

⸻

Final Deliverables

Create the following files:

docs/audits/autonomy/
├── README.md
├── 01-execution-architecture.md
├── 02-controller-analysis.md
├── 03-approval-analysis.md
├── 04-recovery-analysis.md
├── 05-conversation-attention.md
├── 06-benchmark-results.md
├── 07-root-cause-analysis.md
├── 08-remediation-roadmap.md
└── 09-sdk-integration.md       (Revision 2)

The README.md must provide an executive summary answering five questions:

1. Why does Otter feel less autonomous than native Claude Code?
2. Which components introduce the most overhead?
3. Why does Otter request so much user attention?
4. What are the three highest-impact improvements?
5. Which parts of the existing architecture should be simplified or removed?

Completion Criteria

The audit is complete only when:

- The actual execution flow has been documented.
- Controller intervention points have been identified.
- Approval behavior has been traced.
- Timeout and recovery behavior has been analyzed.
- User-visible message sources have been classified.
- Native versus managed execution has been compared where feasible.
- Major conclusions have supporting evidence.
- A prioritized remediation roadmap exists.

If runtime benchmarks cannot be completed, clearly distinguish code-review findings from experimentally validated conclusions.

Do not fabricate benchmark results.

Do not claim an architectural problem is confirmed without supporting evidence.

Explicit Non-Goals

Do not:

- Redesign the desktop UI.
- Implement a new Controller.
- Replace the existing agent runtime.
- Change permission defaults.
- Refactor unrelated code.
- Introduce new orchestration frameworks.
- Modify production behavior during the audit.

The purpose of this investigation is to determine what should change before changing it.
