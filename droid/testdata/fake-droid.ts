#!/usr/bin/env bun

import { createInterface } from "node:readline";

// A minimal `droid exec --input-format stream-jsonrpc` stand-in. It sends
// the two interactive server requests after session start, reports the
// client's answers as notifications, and never answers never_respond.
const input = createInterface({ input: process.stdin });

input.on("line", (line) => {
  const message = JSON.parse(line) as Record<string, unknown>;
  if (message.type === "response") {
    notify({ type: "test_server_response", response: message });
    return;
  }
  if (message.method === "droid.initialize_session") {
    write({ type: "response", id: message.id, result: { sessionId: "droid_test_session" } });
    write({ type: "request", id: "perm-1", method: "droid.request_permission", params: { toolUses: [], options: [{ value: "proceed_once" }, { value: "cancel" }] } });
    write({ type: "request", id: "ask-1", method: "droid.ask_user", params: { toolCallId: "tool-ask", questions: [] } });
    write({ type: "request", id: "future-1", method: "droid.future_prompt", params: {} });
    return;
  }
  if (message.method === "droid.fail") {
    write({ type: "response", id: message.id, error: { code: -32004, message: "session not found" } });
    return;
  }
  if (message.method !== "droid.never_respond") {
    write({ type: "response", id: message.id, result: {} });
  }
});

function notify(notification: Record<string, unknown>): void {
  write({ type: "notification", method: "droid.session_notification", params: { sessionId: "droid_test_session", notification } });
}

function write(message: Record<string, unknown>): void {
  process.stdout.write(`${JSON.stringify({ jsonrpc: "2.0", factoryApiVersion: "1.0.0", ...message })}\n`);
}
