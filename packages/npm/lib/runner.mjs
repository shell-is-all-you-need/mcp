import { createHash } from "node:crypto";
import { existsSync, mkdirSync, chmodSync, readFileSync, renameSync, rmSync, writeFileSync } from "node:fs";
import { get } from "node:https";
import { homedir, platform, arch } from "node:os";
import { dirname, join } from "node:path";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const packageJson = JSON.parse(
  readFileSync(new URL("../package.json", import.meta.url), "utf8"),
);
const VERSION = packageJson.version;
function releaseRepository() {
  if (process.env.SHELL_IS_ALL_YOU_NEED_GITHUB_REPOSITORY) return process.env.SHELL_IS_ALL_YOU_NEED_GITHUB_REPOSITORY;
  const url = packageJson.repository?.url ?? "";
  const match = url.match(/github\.com\/([^/]+\/[^/.]+)(?:\.git)?$/);
  if (!match || match[1] === "__GITHUB_REPOSITORY__") {
    throw new Error("release repository is not configured; set SHELL_IS_ALL_YOU_NEED_GITHUB_REPOSITORY=owner/repo");
  }
  return match[1];
}

function target() {
  const key = `${platform()}/${arch()}`;
  const targets = {
    "linux/x64": "x86_64-unknown-linux-musl",
    "linux/arm64": "aarch64-unknown-linux-musl",
    "darwin/x64": "x86_64-apple-darwin",
    "darwin/arm64": "aarch64-apple-darwin",
    "win32/x64": "x86_64-pc-windows-msvc",
    "win32/arm64": "aarch64-pc-windows-msvc",
  };
  const value = targets[key];
  if (!value) {
    throw new Error(`unsupported platform: ${key}`);
  }
  return value;
}

function cacheRoot() {
  if (process.env.SHELL_IS_ALL_YOU_NEED_CACHE_DIR) return process.env.SHELL_IS_ALL_YOU_NEED_CACHE_DIR;
  if (platform() === "win32" && process.env.LOCALAPPDATA) {
    return join(process.env.LOCALAPPDATA, "shell-is-all-you-need");
  }
  if (platform() === "darwin") {
    return join(homedir(), "Library", "Caches", "shell-is-all-you-need");
  }
  return join(process.env.XDG_CACHE_HOME ?? join(homedir(), ".cache"), "shell-is-all-you-need");
}

function download(url, redirects = 5) {
  return new Promise((resolve, reject) => {
    const request = get(
      url,
      { headers: { "User-Agent": `shell-is-all-you-need-npm/${VERSION}` } },
      (response) => {
        const status = response.statusCode ?? 0;
        if (status >= 300 && status < 400 && response.headers.location && redirects > 0) {
          response.resume();
          resolve(download(new URL(response.headers.location, url).toString(), redirects - 1));
          return;
        }
        if (status !== 200) {
          response.resume();
          reject(new Error(`download failed (${status}): ${url}`));
          return;
        }
        const chunks = [];
        response.on("data", (chunk) => chunks.push(chunk));
        response.on("end", () => resolve(Buffer.concat(chunks)));
      },
    );
    request.on("error", reject);
    request.setTimeout(60_000, () => {
      request.destroy(new Error(`download timed out: ${url}`));
    });
  });
}

async function installedBinary() {
  if (process.env.SHELL_IS_ALL_YOU_NEED_BINARY) return process.env.SHELL_IS_ALL_YOU_NEED_BINARY;

  const triple = target();
  const extension = platform() === "win32" ? ".exe" : "";
  const asset = `shell-is-all-you-need-${triple}${extension}`;
  const destination = join(cacheRoot(), VERSION, asset);
  if (existsSync(destination)) return destination;

  mkdirSync(dirname(destination), { recursive: true });
  const base = `https://github.com/${releaseRepository()}/releases/download/v${VERSION}/${asset}`;
  const [binary, checksumFile] = await Promise.all([
    download(base),
    download(`${base}.sha256`),
  ]);
  const expected = checksumFile.toString("utf8").trim().split(/\s+/)[0];
  if (!/^[0-9a-f]{64}$/i.test(expected)) {
    throw new Error("release checksum has an invalid format");
  }
  const actual = createHash("sha256").update(binary).digest("hex");
  if (actual.toLowerCase() !== expected.toLowerCase()) {
    throw new Error(`SHA-256 mismatch for ${asset}`);
  }

  const temporary = `${destination}.${process.pid}.tmp`;
  try {
    writeFileSync(temporary, binary, { mode: 0o755 });
    if (platform() !== "win32") chmodSync(temporary, 0o755);
    renameSync(temporary, destination);
  } finally {
    rmSync(temporary, { force: true });
  }
  return destination;
}

export async function run(args = []) {
  const binary = await installedBinary();
  const result = spawnSync(binary, args, { stdio: "inherit" });
  if (result.error) throw result.error;
  process.exitCode = result.status ?? 1;
}
