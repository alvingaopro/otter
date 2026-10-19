import { describe, expect, it } from "vitest";
import { installing, offered, progressed, updateLabel } from "./update";

describe("the app's update (D-125)", () => {
  it("is offered by a check, and gone when the next finds nothing", () => {
    const offer = offered(null, "0.8.0");
    expect(offer).toEqual({ version: "0.8.0" });
    expect(offered(offer, "0.8.0")).toBe(offer);
    expect(offered(offer, "0.8.1")).toEqual({ version: "0.8.1" });
    expect(offered(offer, null)).toBeNull();
  });

  it("says how far installing has got", () => {
    const offer = { version: "0.8.0" };
    expect(updateLabel(offer)).toBe("Update to v0.8.0");
    expect(updateLabel(progressed(offer, 0, 0)!)).toBe("Updating…");
    expect(updateLabel(progressed(offer, 42, 100)!)).toBe("Updating… 42%");
    expect(updateLabel(progressed(offer, 100, 100)!)).toBe("Installing…");
    expect(progressed(null, 1, 2)).toBeNull();
  });

  it("keeps an install under way through a check, and offers a retry after a failure", () => {
    const busy = progressed({ version: "0.8.0" }, 1, 2)!;
    expect(installing(busy)).toBe(true);
    expect(offered(busy, null)).toBe(busy);
    const failed = { version: "0.8.0", error: "couldn't update to v0.8.0: no space left" };
    expect(installing(failed)).toBe(false);
    expect(updateLabel(failed)).toBe("Update failed — try again");
  });
});
