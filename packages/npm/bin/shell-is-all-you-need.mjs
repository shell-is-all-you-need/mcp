#!/usr/bin/env node
import { run } from "../lib/runner.mjs";

await run(process.argv.slice(2));
