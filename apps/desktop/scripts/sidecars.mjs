// Builds `otter` and `otterd` from the main workspace and puts them where
// Tauri's `externalBin` expects them (`src-tauri/binaries/<name>-<triple>`),
// so the .app carries the command-line tools of its own version (D-085).
// Tauri runs this before `tauri dev` / `tauri build`, with the target and
// profile in TAURI_ENV_TARGET_TRIPLE / TAURI_ENV_DEBUG.
import { execFileSync } from "node:child_process";
import { copyFileSync, mkdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const desktop = join(dirname(fileURLToPath(import.meta.url)), "..");
const root = join(desktop, "../..");
const host = execFileSync("rustc", ["-vV"], { encoding: "utf8" }).match(/^host: (\S+)$/m)[1];
const triple = process.env.TAURI_ENV_TARGET_TRIPLE || host;
const debug = process.env.TAURI_ENV_DEBUG === "true";

const args = ["build", "--locked", "-p", "otterd", "-p", "otter"];
if (!debug) args.push("--release");
// The host's own target builds into the usual directory, shared with `cargo build`.
if (triple !== host) args.push("--target", triple);
execFileSync("cargo", args, { cwd: root, stdio: "inherit" });

const target = process.env.CARGO_TARGET_DIR || join(root, "target");
const built = join(target, triple === host ? "" : triple, debug ? "debug" : "release");
const out = join(desktop, "src-tauri/binaries");
mkdirSync(out, { recursive: true });
for (const bin of ["otter", "otterd"]) {
  copyFileSync(join(built, bin), join(out, `${bin}-${triple}`));
}
