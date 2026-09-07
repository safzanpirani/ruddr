#!/usr/bin/env bun

import { createEmitter, runAppServer } from "../adapter/protocol";
import { ClaudeRuddrAdapter } from "./runtime";

const emit = createEmitter();
await runAppServer(new ClaudeRuddrAdapter(emit), emit);
