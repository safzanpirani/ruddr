import { promptRoute, type PromptRoute, type Session } from "./format";

export interface DraftTarget {
  stateDir: string;
  route: PromptRoute;
  turnId?: string;
}

/** A nonempty draft retains its delivery intent across live session updates. */
export function draftTarget(session: Session | undefined, existing: DraftTarget | undefined, hasText: boolean): DraftTarget | undefined {
  if (!session) return undefined;
  if (hasText && existing?.stateDir === session.stateDir) return existing;
  const route = promptRoute(session);
  return route ? { stateDir: session.stateDir, route, turnId: route === "steer" ? session.turnId : undefined } : undefined;
}
