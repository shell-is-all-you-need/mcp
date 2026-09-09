import assert from "node:assert/strict";
import { EventEmitter } from "node:events";
import { mkdtempSync, rmSync } from "node:fs";
import https from "node:https";
import { syncBuiltinESMExports } from "node:module";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { PassThrough } from "node:stream";
import { test } from "node:test";
import { run } from "../packages/npm/lib/runner.mjs";

test("interrupted downloads reject without crashing or caching a partial binary", async () => {
  const cache = mkdtempSync(join(tmpdir(), "shell-is-all-you-need-npm-"));
  const previousCache = process.env.SHELL_IS_ALL_YOU_NEED_CACHE_DIR;
  const previousBinary = process.env.SHELL_IS_ALL_YOU_NEED_BINARY;
  const originalGet = https.get;
  process.env.SHELL_IS_ALL_YOU_NEED_CACHE_DIR = cache;
  delete process.env.SHELL_IS_ALL_YOU_NEED_BINARY;
  https.get = (_url, _options, callback) => {
    const request = new EventEmitter();
    request.setTimeout = () => request;
    queueMicrotask(() => {
      const response = new PassThrough();
      response.statusCode = 200;
      callback(response);
      response.write("partial download");
      response.destroy(new Error("connection interrupted"));
    });
    return request;
  };
  syncBuiltinESMExports();
  try {
    await assert.rejects(run(), /connection interrupted/);
    // A second attempt must download again instead of using partial contents.
    await assert.rejects(run(), /connection interrupted/);
  } finally {
    https.get = originalGet;
    syncBuiltinESMExports();
    if (previousCache === undefined) delete process.env.SHELL_IS_ALL_YOU_NEED_CACHE_DIR;
    else process.env.SHELL_IS_ALL_YOU_NEED_CACHE_DIR = previousCache;
    if (previousBinary === undefined) delete process.env.SHELL_IS_ALL_YOU_NEED_BINARY;
    else process.env.SHELL_IS_ALL_YOU_NEED_BINARY = previousBinary;
    rmSync(cache, { recursive: true, force: true });
  }
});
