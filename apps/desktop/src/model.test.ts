import { describe, expect, it } from "vitest";
import { dropPlace, glance, movePin, moveTo, tabName, trayLabels } from "./model";

const line = (name: string, what: string, host = "code", key = name) => ({ key, name, what, host });

describe("menu bar glance", () => {
  it("says what to do, with only a file's name", () => {
    expect(
      glance(
        "claude needs your approval: Read: /home/alvin/Work/marketplace/cs-96847/apps/salesforce/force-app/main/default/lwc/createOffer/createOffer.html",
      ),
    ).toBe("Approve: Read createOffer.html");
    expect(glance("codex needs your approval: Bash: rm -rf ./build/out")).toBe("Approve: Bash rm -rf out");
    expect(glance("waiting for input (+2)")).toBe("waiting for input (+2)");
  });

  it("says a turn finished, not what it said", () => {
    expect(glance("claude-2 finished: Yes, it would exceed the request limit.\nSo I split it.")).toBe("claude-2 finished");
    expect(glance("claude-2 finished: Yes, it would exceed the request limit (+1)")).toBe("claude-2 finished (+1)");
    expect(glance("claude finished its turn")).toBe("claude finished its turn");
  });

  it("fits a menu", () => {
    const [{ label }] = trayLabels([line("n".repeat(100), `Approve: Bash ${"x".repeat(200)} (+3)`)]);
    expect(label.length).toBeLessThanOrEqual(44);
    expect(label.endsWith("… (+3)")).toBe(true);
    expect(trayLabels([line("marketplace", "claude-2 finished: Yes, it would exceed the request limit")])[0].label).toBe(
      "marketplace · claude-2 finished",
    );
  });

  it("names the host only when another host has the same name", () => {
    const labels = trayLabels([
      line("api", "agent finished", "mac", "a"),
      line("api", "build failed (status 1)", "box", "b"),
      line("web", "agent finished", "box", "c"),
    ]).map((l) => l.label);
    expect(labels).toEqual(["api (mac) · agent finished", "api (box) · build failed (status 1)", "web · agent finished"]);
  });
});

describe("putting things in order", () => {
  it("moves one among the others", () => {
    expect(moveTo(["a", "b", "c"], "c", 0)).toEqual(["c", "a", "b"]);
    expect(moveTo(["a", "b", "c"], "a", 1)).toEqual(["b", "a", "c"]);
    expect(moveTo(["a", "b", "c"], "a", 9)).toEqual(["b", "c", "a"]);
    // A pin whose workspace isn't loaded keeps its place.
    expect(movePin(["a", "x", "b", "c"], ["a", "b", "c"], "c", 0)).toEqual(["c", "x", "a", "b"]);
  });

  // Two rows of tabs 80 wide: a b c on the first, d on the second.
  const tab = (col: number, row: number) => ({ left: col * 82, top: row * 38, width: 80, bottom: row * 38 + 32 });
  const others = [tab(0, 0), tab(1, 0), tab(2, 0), tab(0, 1)];

  it("drops a tab in reading order, over rows", () => {
    expect(dropPlace(others, 10, 16)).toBe(0);
    expect(dropPlace(others, 100, 16)).toBe(1);
    expect(dropPlace(others, 400, 16)).toBe(3);
    expect(dropPlace(others, 10, 50)).toBe(3);
    expect(dropPlace(others, 60, 50)).toBe(4);
  });

  it("takes a pointer above or below the tabs as on the first or last row", () => {
    expect(dropPlace(others, 100, -30)).toBe(1);
    expect(dropPlace(others, 10, 400)).toBe(3);
    expect(dropPlace([tab(0, 0), tab(1, 0)], 100, 300)).toBe(1);
    expect(dropPlace([], 100, 16)).toBe(0);
  });
});

describe("a session's tab", () => {
  it("cuts a long name (an agent's title, D-196) at 40 characters", () => {
    expect(tabName("claude")).toBe("claude");
    const title = "Chat 聊天框 ／ 命令自动补全，再加上一个很长很长的说明文字，一直写到超过四十个字符为止";
    const cut = tabName(title);
    expect([...cut]).toHaveLength(40);
    expect(cut.endsWith("…")).toBe(true);
    expect(title.startsWith(cut.slice(0, -1))).toBe(true);
    expect(tabName("x".repeat(40))).toBe("x".repeat(40));
  });
});
