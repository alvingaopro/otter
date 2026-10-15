// Requests an older otterd doesn't know (D-106): asked once per host.

import { describe, expect, it, vi } from "vitest";

const invoke = vi.hoisted(() => vi.fn());
vi.mock("@tauri-apps/api/core", () => ({ invoke }));

describe("older hosts", () => {
  it("are asked once, then left alone until they come back", async () => {
    const { invokeNewer, forgetHost, explain } = await import("./newer");
    invoke.mockImplementation(async () => {
      throw "unknown variant `workspace.changes`, expected one of …";
    });
    expect(await invokeNewer("old", "workspace_changes", {})).toBeUndefined();
    expect(await invokeNewer("old", "workspace_changes", {})).toBeUndefined();
    expect(invoke).toHaveBeenCalledTimes(1);
    invoke.mockImplementation(async () => ({ git: true }));
    forgetHost("old");
    expect(await invokeNewer("old", "workspace_changes", {})).toEqual({ git: true });
    invoke.mockImplementation(async () => {
      throw new Error("permission denied");
    });
    // Any other error is the caller's to show.
    const failed = await invokeNewer("new", "workspace_changes", {}).then(
      () => "",
      (e: unknown) => String(e),
    );
    expect(failed).toContain("permission denied");
    expect(explain("old", "unknown variant `fs.remove`")).toContain("too old");
  });
});

