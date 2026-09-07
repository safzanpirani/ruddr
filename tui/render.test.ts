import { expect, test } from "bun:test";
import { TextRenderable } from "@opentui/core";
import { createTestRenderer } from "@opentui/core/testing";
import { renderUsage } from "./render";
import type { Session } from "./core";

for (const width of [40, 120]) {
  test(`usage footer renders current context at ${width} columns`, async () => {
    const setup = await createTestRenderer({ width, height: 3 });
    const session: Session = {
      version: 2, pid: 1, status: "completed", stateDir: "/test", stateFile: "/test/state.json",
      tokenUsage: { totalTokens: 2_500_000, contextTokens: 50_000, contextWindow: 200_000 },
    };
    const text = new TextRenderable(setup.renderer, {
      id: "usage", content: renderUsage(session), width: "100%", height: 1, wrapMode: "none",
    });
    setup.renderer.root.add(text);
    try {
      await setup.renderOnce();
      expect(setup.captureCharFrame()).toContain("50.0K · 25% · completed");
      expect(setup.captureCharFrame()).not.toContain("2.5M");
      session.tokenUsage!.contextTokens = undefined;
      text.content = renderUsage(session);
      await setup.renderOnce();
      expect(setup.captureCharFrame()).toContain("2.5M total · completed");
      expect(setup.captureCharFrame()).not.toContain("100%");
    } finally {
      setup.renderer.destroy();
    }
  });
}
