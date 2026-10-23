import { describe, expect, it } from "vitest";
import { sharing, slackManifest } from "./slackManifest";

describe("A Slack app per host (D-203)", () => {
  it("names the app and its bot after the host, within Slack's limits", () => {
    const m = JSON.parse(slackManifest("gpu"));
    expect(m.display_information.name).toBe("Otter (gpu)");
    expect(m.features.bot_user.display_name).toBe("Otter-gpu");
    expect(m.settings.socket_mode_enabled).toBe(true);
    expect(m.oauth_config.scopes.bot).toContain("files:read");
    expect(m.features.slash_commands[0].command).toBe("/otter");
    const long = JSON.parse(slackManifest("a very long host name indeed, with spaces"));
    expect(long.display_information.name.length).toBeLessThanOrEqual(35);
    expect(long.features.bot_user.display_name).toMatch(/^[A-Za-z0-9._-]+$/);
  });

  it("finds the other hosts connected as the same app", () => {
    const me = { team: "Suger Inc", bot_id: "B1" };
    const others = [
      { host: "gpu", slack: { team: "Suger Inc", bot_id: "B1", state: "connected" } },
      { host: "dev", slack: { team: "Suger Inc", bot_id: "B2", state: "connected" } },
      { host: "old" },
    ];
    expect(sharing(me, others)).toEqual(["gpu"]);
    expect(sharing({ team: "Suger Inc" }, others)).toEqual([]);
  });
});
