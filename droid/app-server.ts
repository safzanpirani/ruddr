#!/usr/bin/env bun

import { createEmitter, runAppServer } from "../adapter/protocol";
import { DroidRuddrAdapter } from "./runtime";

const emit = createEmitter();
await runAppServer(new DroidRuddrAdapter(emit), emit);
