import { describe, expect, it } from "vitest";
import { MAX_SIDE, fitWithin, pastedFiles } from "./pastedImage";

describe("fitWithin", () => {
  it("scales the longest side down to the limit and keeps the shape", () => {
    expect(fitWithin(3200, 1600)).toEqual({ width: MAX_SIDE, height: 800 });
    expect(fitWithin(1000, 4000)).toEqual({ width: 400, height: MAX_SIDE });
  });
  it("never scales up", () => {
    expect(fitWithin(800, 600)).toEqual({ width: 800, height: 600 });
  });
});

describe("pastedFiles", () => {
  const file = new File(["x"], "s.png", { type: "image/png" });
  const data = (items: object[]) => ({ items }) as unknown as DataTransfer;
  it("takes the pictures of a clipboard that holds pictures", () => {
    expect(pastedFiles(data([{ kind: "file", type: "image/png", getAsFile: () => file }]))).toEqual([file]);
  });
  it("leaves a clipboard with text to the text, and one with no picture alone", () => {
    expect(pastedFiles(data([{ kind: "string", type: "text/plain" }, { kind: "file", type: "image/png", getAsFile: () => file }]))).toEqual([]);
    expect(pastedFiles(data([{ kind: "file", type: "application/pdf", getAsFile: () => file }]))).toEqual([]);
    expect(pastedFiles(null)).toEqual([]);
  });
});
