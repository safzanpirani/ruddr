import { promptRoute, type PromptRoute, type Session } from "./format";

export interface DraftTarget {
  stateDir: string;
  route: PromptRoute;
  turnId?: string;
  threadId?: string;
}

/** A nonempty draft retains its delivery intent across live session updates. */
export function draftTarget(session: Session | undefined, existing: DraftTarget | null | undefined, hasText: boolean): DraftTarget | undefined {
  if (!session) return undefined;
  if (hasText && existing === null) return undefined;
  if (hasText && existing) return existing.stateDir === session.stateDir ? existing : undefined;
  const route = promptRoute(session);
  return route ? { stateDir: session.stateDir, route, threadId: session.threadId, turnId: route === "steer" ? session.turnId : undefined } : undefined;
}

export interface SavedDraft {
  text: string;
  target: DraftTarget | null;
}

/** Untrusted and legacy storage never acquires a destination during restore. */
export function restoreDraft(value: unknown, session: Session | undefined): SavedDraft {
  if (typeof value === "string") return { text: value, target: null };
  if (!value || typeof value !== "object" || !("text" in value) || typeof value.text !== "string")
    return { text: "", target: null };
  const text = value.text;
  const target = "target" in value ? value.target : undefined;
  if (!session || !target || typeof target !== "object" || !("stateDir" in target) || !("route" in target))
    return { text, target: null };
  const current = draftTarget(session, undefined, false);
  if (!current || target.stateDir !== current.stateDir || target.route !== current.route ||
      !("threadId" in target) || target.threadId !== current.threadId ||
      (current.route === "steer" && (!("turnId" in target) || target.turnId !== current.turnId)))
    return { text, target: null };
  return { text, target: current };
}
