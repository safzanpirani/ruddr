import { expect, test } from "bun:test";
import { draftTarget, restoreDraft } from "./prompt";
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
  expect(draftTarget({ ...session, stateDir: "/other", turnId: "two" }, target, true)).toBeUndefined();
});

const roundTrip = (value: unknown) => JSON.parse(JSON.stringify(value));

test("persisted steer restores the exact request target only for the same active turn", () => {
  const saved = roundTrip({ text: "keep this draft", target: draftTarget(session, undefined, true) });
  expect(restoreDraft(saved, session)).toEqual(saved);
  for (const changed of [
    { ...session, turnId: "two" }, { ...session, status: "idle" },
    { ...session, status: "completed" }, { ...session, stateDir: "/other" },
    { ...session, threadId: "replacement" }, undefined,
  ]) {
    const restored = restoreDraft(saved, changed);
    expect(restored).toEqual({ text: saved.text, target: null });
    expect(draftTarget(changed, restored.target, true)).toBeUndefined();
  }
});

test("idle and continuation drafts keep their route across reloads", () => {
  for (const status of ["idle", "completed"]) {
    const current = { ...session, status };
    const saved = roundTrip({ text: "follow up", target: draftTarget(current, undefined, true) });
    expect(restoreDraft(saved, current)).toEqual(saved);
    expect(restoreDraft(saved, session).target).toBeNull();
  }
});

test("legacy, malformed and already unarmed drafts stay unarmed through typing and another reload", () => {
  for (const value of ["legacy text", { text: "unarmed", target: null }, { text: "bad target", target: { route: "steer", stateDir: session.stateDir } }]) {
    const restored = restoreDraft(value, session);
    expect(restored.target).toBeNull();
    expect(draftTarget(session, restored.target, true)).toBeUndefined();
    expect(restoreDraft(roundTrip(restored), session)).toEqual(restored);
    expect(draftTarget(session, restored.target, false)).toMatchObject({ route: "steer", turnId: "one" });
  }
  for (const value of [null, 42, {}, { text: 42 }]) expect(restoreDraft(value, session).text).toBe("");
});
