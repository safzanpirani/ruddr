import { expect, test } from "bun:test";
import { PromptSubmission } from "./prompt";

test("a failed send preserves the draft and an explicit retry clears it on acceptance", async () => {
  let text = "  retry this prompt\n";
  let closes = 0;
  const submission = new PromptSubmission(() => text, () => { text = ""; closes++; });
  await expect(submission.submit(async () => { throw new Error("steer rejected"); }))
    .rejects.toThrow("steer rejected");
  expect(text).toBe("  retry this prompt\n");
  expect(closes).toBe(0);
  await submission.submit(async () => "accepted");
  expect(text).toBe("");
  expect(closes).toBe(1);
});

test("late acceptance preserves edits, reopened prompts, and shutdown", async () => {
  for (const change of ["edit", "reopen", "shutdown"]) {
    let text = "original";
    let closes = 0;
    const submission = new PromptSubmission(() => text, () => { closes++; });
    const gate = Promise.withResolvers<void>();
    const pending = submission.submit(() => gate.promise);
    if (change === "edit") text = "newer draft";
    else submission.invalidate();
    gate.resolve();
    await pending;
    expect(closes).toBe(0);
  }
});
