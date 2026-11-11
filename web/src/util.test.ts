import { describe, expect, it } from "vitest";
import { crumbs, formatBytes, join, parseAmzDate, relativeTime, savings, sortEntries } from "./util";

describe("formatting", () => {
  it("bytes", () => {
    expect(formatBytes(0)).toBe("0 B");
    expect(formatBytes(1536)).toBe("1.5 KiB");
    expect(formatBytes(5 * 1024 ** 3)).toBe("5.0 GiB");
    expect(formatBytes(300 * 1024 ** 2)).toBe("300 MiB");
  });
  it("snapshot times", () => {
    expect(parseAmzDate("20261110T210459Z").toISOString()).toBe("2026-11-10T21:04:59.000Z");
    expect(Number.isNaN(parseAmzDate("yesterday").getTime())).toBe(true);
  });
  it("savings", () => {
    expect(savings(1000, 0)).toBe("100.00%");
    expect(savings(1000, 250)).toBe("75.0%");
    expect(savings(0, 0)).toBe("–");
  });
  it("relative time", () => {
    const now = new Date(0);
    expect(relativeTime(new Date(90 * 60 * 1000), now)).toBe("in 2 h");
    expect(relativeTime(new Date(-30 * 1000), now)).toBe("30 s ago");
  });
});

describe("paths", () => {
  it("breadcrumbs and joins", () => {
    expect(crumbs("a/b/c")).toEqual([["a", "a"], ["b", "a/b"], ["c", "a/b/c"]]);
    expect(crumbs("")).toEqual([]);
    expect(join("", "x")).toBe("x");
    expect(join("a/b", "x")).toBe("a/b/x");
  });
  it("directories first", () => {
    const e = sortEntries([{ name: "b.txt", type: "file" }, { name: "z", type: "dir" }, { name: "a.txt", type: "file" }]);
    expect(e.map((x) => x.name)).toEqual(["z", "a.txt", "b.txt"]);
  });
});
