import { expect, test } from "bun:test";
import { ansiSegments } from "./ansi";

test("maps SGR colors and drops other escapes", () => {
  expect(ansiSegments("\x1b[1;32mok\x1b[0m done\x1b[2K\x1b]0;title\x07!")).toEqual([
    { text: "ok", className: "ansi-green ansi-bold" },
    { text: " done!", className: "" },
  ]);
  expect(ansiSegments("\x1b[38;5;196mred\x1b[39m plain")).toEqual([{ text: "red plain", className: "" }]);
  expect(ansiSegments("<b>x</b>")).toEqual([{ text: "<b>x</b>", className: "" }]);
});
