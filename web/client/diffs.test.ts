import { expect, test } from "bun:test";
import { fileDiffForEdit } from "./diffs";
import { editsFromPatchText, fileEditsFromItem, unifiedPatchForEdit } from "./transcript";

test("Pierre cache keys change when an edit snapshot changes", () => {
  const old = fileDiffForEdit({ path: "x.ts", kind: "update", oldText: "a", newText: "b" }, "same-item");
  const next = fileDiffForEdit({ path: "x.ts", kind: "update", oldText: "a", newText: "c" }, "same-item");
  expect(old).toBeDefined();
  expect(next).toBeDefined();
  expect(old!.cacheKey).not.toBe(next!.cacheKey);
});

test("replacement fragments omit false EOF markers without changing whole-file diffs", () => {
  const edit = { path: "x.ts", kind: "update", oldText: "old", newText: "new" };
  const fragment = fileDiffForEdit({ ...edit, fragment: true }, "fragment")!;
  expect(fragment.hunks[0].noEOFCRAdditions).not.toBe(true);
  expect(fragment.hunks[0].noEOFCRDeletions).not.toBe(true);
  const wholeFile = fileDiffForEdit(edit, "whole-file")!;
  expect(wholeFile.hunks[0].noEOFCRAdditions).toBe(true);
  expect(wholeFile.hunks[0].noEOFCRDeletions).toBe(true);
  const deletion = fileDiffForEdit({ ...edit, newText: "", fragment: true }, "delete-fragment")!;
  expect(deletion.additionLines).toHaveLength(0);
  const newlineEdit = fileDiffForEdit({ ...edit, oldText: "same\n", newText: "same", fragment: true }, "newline-edit")!;
  expect(newlineEdit.deletionLines).toEqual(["same\n"]);
  expect(newlineEdit.additionLines).toEqual(["same"]);
});

test("splits a workspace diff into stable per-file patches", async () => {
  const { patchesByPath } = await import("./diffs");
  const a = "diff --git a/x.ts b/x.ts\n--- a/x.ts\n+++ b/x.ts\n@@ -1 +1 @@\n-a\n+b\n";
  const b = "diff --git a/y.ts b/y.ts\nnew file mode 100644\n--- /dev/null\n+++ b/y.ts\n@@ -0,0 +1 @@\n+c\n";
  const patches = patchesByPath(a + b);
  expect([...patches.keys()]).toEqual(["x.ts", "y.ts"]);
  expect(patches.get("x.ts")).toBe(a);
  expect(patchesByPath(a + b.replace("+c", "+d")).get("x.ts")).toBe(a);
});

test("unnumbered hunks preserve every line with accurate fragment counts", () => {
  const edit = editsFromPatchText("*** Begin Patch\n*** Update File: x.ts\n@@ function first\n context\n-old\n+new\n+extra\n@@\n-gone\n-also gone\n+replacement\n*** End Patch")[0];
  expect(edit.fragment).toBe(true);
  expect(unifiedPatchForEdit(edit)).toContain("@@ -1,2 +1,3 @@");
  expect(unifiedPatchForEdit(edit)).toContain("@@ -3,2 +4,1 @@");
  const parsed = fileDiffForEdit(edit, "fragments")!;
  expect(parsed.hunks).toHaveLength(2);
  expect(parsed.hunks.map(h => [h.deletionCount, h.additionCount])).toEqual([[2, 3], [2, 1]]);
  expect(parsed.additionLines.join("")).toContain("replacement");
  expect(parsed.deletionLines.join("")).toContain("also gone");
  expect(parsed.hunks.every(h => !h.noEOFCRAdditions && !h.noEOFCRDeletions)).toBe(true);
});

test("provider bare patches are fragments; numbered source coordinates are preserved", () => {
  const edit = fileEditsFromItem({ changes: [{ path: "x.ts", diff: "@@\n-a\n+b\n+c\n" }] })[0];
  expect(edit.fragment).toBe(true);
  expect(fileDiffForEdit(edit, "bare")!.hunks[0].additionCount).toBe(2);
  const numbered = editsFromPatchText("*** Begin Patch\n*** Update File: x.ts\n@@ -42,1 +42,2 @@\n-a\n+b\n+c\n*** End Patch")[0];
  expect(numbered.fragment).toBeUndefined();
  expect(fileDiffForEdit(numbered, "numbered")!.hunks[0].deletionStart).toBe(42);
});

test("pure additions and deletions inside fragments have correct empty-side counts", () => {
  for (const body of ["+one\n+two", "-one\n-two"]) {
    const edit = editsFromPatchText(`*** Begin Patch\n*** Update File: x.ts\n@@\n${body}\n*** End Patch`)[0];
    const hunk = fileDiffForEdit(edit, "one-sided")!.hunks[0];
    expect([hunk.deletionCount, hunk.additionCount]).toEqual(body[0] === "+" ? [0, 2] : [2, 0]);
  }
});

test("unknown writes have no manufactured diff metadata", () => {
  expect(fileDiffForEdit({ path: "x.ts", kind: "write", newText: "replacement" }, "write")).toBeUndefined();
});
