import { expect, test } from "bun:test";
import { draftTarget } from "./prompt";
import type { Session } from "./format";

const session: Session = { version: 2, pid: 1, stateDir: "/run", stateFile: "/run/state.json", threadId: "t", turnId: "one", cwd: "/workspace", status: "active" };

test("a typed steer retains its route and turn ID across idle, completion, and a new turn", () => {
  const target = draftTarget(session, undefined, true);
  for (const status of ["idle", "completed", "active"]) {
    expect(draftTarget({ ...session, status, turnId: "two" }, target, true)).toEqual(target);
  }
  expect(draftTarget({ ...session, status: "idle" }, target, false)?.route).toBe("prompt");
});

test("draft targets cannot move to a different session", () => {
  const target = draftTarget(session, undefined, true);
  expect(draftTarget({ ...session, stateDir: "/other", turnId: "two" }, target, true)).toMatchObject({ stateDir: "/other", turnId: "two" });
});
