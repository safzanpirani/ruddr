import { expect, test } from "bun:test";
import { fileDiffForEdit } from "./diffs";

test("Pierre cache keys change when an edit snapshot changes", () => {
  const old = fileDiffForEdit({ path: "x.ts", kind: "update", oldText: "a", newText: "b" }, "same-item");
  const next = fileDiffForEdit({ path: "x.ts", kind: "update", oldText: "a", newText: "c" }, "same-item");
  expect(old).toBeDefined();
  expect(next).toBeDefined();
  expect(old!.cacheKey).not.toBe(next!.cacheKey);
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
