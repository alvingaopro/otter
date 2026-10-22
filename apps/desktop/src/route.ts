/**
 * Which host a request is about (D-179). The same rules as
 * `crates/client/src/route.rs`; change both. A message that names a
 * workspace, or the repository one comes from, belongs to the host that has
 * it. No model is asked: a wrong guess costs a switch of host, not a task.
 */

export interface RoutedWorkspace {
  name: string;
  /** A Git workspace's repository, without credentials. */
  repository?: string;
}

export type Route =
  | { kind: "one"; host: string; why: string }
  | { kind: "several"; hosts: string[] }
  | { kind: "nowhere" };

const STRONG = 2;
const WEAK = 1;
/** Shortest name taken for a word on its own: shorter ones (`api`, `web`, `add`) are in every sentence. */
const MIN_NAME = 4;
const MIN_REPOSITORY_WORD = 4;

/** `acme/billing` for `git@github.com:acme/billing.git` and `https://token@github.com/acme/billing/`. */
export function repositoryIdentity(repository: string): { owner?: string; name: string } | undefined {
  let r = repository.trim().replace(/\/+$/, "");
  r = r.replace(/\.git$/, "");
  const parts = r.split(/[/:]/).filter(Boolean);
  const name = parts.pop()?.toLowerCase();
  if (!name) return undefined;
  const owner = parts.pop();
  return { owner: owner && !owner.includes("@") && !owner.includes(".") ? owner.toLowerCase() : undefined, name };
}

/** `needle` in `hay` as a word: not inside a longer name. */
function containsWord(hay: string, needle: string): boolean {
  // Names are ASCII; any other character (a Chinese or Japanese sentence has
  // no spaces) ends one.
  const nameChar = (c: string | undefined) => c !== undefined && /[A-Za-z0-9_-]/.test(c);
  // Whole characters either side, as the Rust side sees them.
  const before = (i: number) => Array.from(hay.slice(0, i)).pop();
  const after = (i: number) => Array.from(hay.slice(i))[0];
  for (let i = hay.indexOf(needle); i >= 0; i = hay.indexOf(needle, i + 1)) {
    if (!nameChar(before(i)) && !nameChar(after(i + needle.length))) return true;
  }
  return false;
}

/** Length in characters, as Rust's `chars().count()`. */
const chars = (s: string) => Array.from(s).length;

function matched(text: string, w: RoutedWorkspace): { score: number; why: string } | undefined {
  const name = w.name.toLowerCase();
  if (chars(name) >= MIN_NAME && containsWord(text, name)) return { score: STRONG, why: `workspace ${w.name}` };
  const id = w.repository ? repositoryIdentity(w.repository) : undefined;
  if (!id) return undefined;
  if (id.owner && containsWord(text, `${id.owner}/${id.name}`)) {
    return { score: STRONG, why: `repository ${id.owner}/${id.name}` };
  }
  if (chars(id.name) >= MIN_REPOSITORY_WORD && containsWord(text, id.name)) {
    return { score: WEAK, why: `repository ${id.name}` };
  }
  return undefined;
}

export function routeHost(text: string, hosts: Record<string, RoutedWorkspace[]>): Route {
  const lower = text.toLowerCase();
  const found: { host: string; score: number; why: string }[] = [];
  for (const [host, workspaces] of Object.entries(hosts)) {
    let best: { score: number; why: string } | undefined;
    for (const w of workspaces) {
      const m = matched(lower, w);
      if (m && (!best || m.score > best.score)) best = m;
    }
    if (best) found.push({ host, ...best });
  }
  const top = Math.max(0, ...found.map((f) => f.score));
  const winners = found.filter((f) => f.score === top);
  if (top === 0) return { kind: "nowhere" };
  if (winners.length === 1) return { kind: "one", host: winners[0].host, why: winners[0].why };
  return { kind: "several", hosts: winners.map((f) => f.host) };
}
