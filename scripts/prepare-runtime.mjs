import { access, chmod, mkdir, readFile, readdir, rename, rm, writeFile } from "node:fs/promises";
import { createWriteStream } from "node:fs";
import { spawn } from "node:child_process";
import { Readable } from "node:stream";
import { finished } from "node:stream/promises";
import { join, resolve } from "node:path";
import { tmpdir } from "node:os";

const NODE_VERSION = "24.14.0";
const DSH_VERSION = "0.1.5-rc.1";
const RUNTIME_ID = `${NODE_VERSION}-${process.platform}-${process.arch}`;
const root = resolve(import.meta.dirname, "..");
const resources = join(root, "src-tauri", "resources");
const nodeTarget = join(resources, "node");
const harnessTarget = join(resources, "harness");
const stamp = join(nodeTarget, ".runtime-version");

function run(command, args, cwd = root) {
  return new Promise((resolvePromise, reject) => {
    const child = spawn(command, args, { cwd, stdio: "inherit" });
    child.on("error", reject);
    child.on("exit", (code) => code === 0 ? resolvePromise() : reject(new Error(`${command} exited with ${code}`)));
  });
}

async function download(url, destination) {
  const response = await fetch(url, { redirect: "follow" });
  if (!response.ok || !response.body) throw new Error(`Download failed (${response.status}): ${url}`);
  await finished(Readable.fromWeb(response.body).pipe(createWriteStream(destination)));
}

async function prepareNode() {
  try {
    if ((await readFile(stamp, "utf8")).trim() === RUNTIME_ID) return;
  } catch {}

  const platformName = { darwin: "darwin", linux: "linux", win32: "win" }[process.platform];
  const archName = { arm64: "arm64", x64: "x64" }[process.arch];
  if (!platformName || !archName) throw new Error(`Unsupported build target: ${process.platform}-${process.arch}`);

  const folder = `node-v${NODE_VERSION}-${platformName}-${archName}`;
  const extension = process.platform === "win32" ? "zip" : "tar.gz";
  const temp = join(tmpdir(), `deepseek-harness-node-${process.pid}`);
  const archive = join(temp, `node.${extension}`);
  await rm(temp, { recursive: true, force: true });
  await mkdir(temp, { recursive: true });
  await download(`https://nodejs.org/dist/v${NODE_VERSION}/${folder}.${extension}`, archive);

  if (process.platform === "win32") {
    await run("powershell", ["-NoProfile", "-Command", `Expand-Archive -LiteralPath '${archive.replaceAll("'", "''")}' -DestinationPath '${temp.replaceAll("'", "''")}' -Force`]);
  } else {
    await run("tar", ["-xzf", archive, "-C", temp]);
  }

  await rm(nodeTarget, { recursive: true, force: true });
  await rename(join(temp, folder), nodeTarget);
  if (process.platform !== "win32") {
    await chmod(join(nodeTarget, "bin", "node"), 0o755);
    await Promise.all([
      rm(join(nodeTarget, "include"), { recursive: true, force: true }),
      rm(join(nodeTarget, "lib"), { recursive: true, force: true }),
      rm(join(nodeTarget, "share"), { recursive: true, force: true }),
      rm(join(nodeTarget, "bin", "corepack"), { force: true }),
      rm(join(nodeTarget, "bin", "npm"), { force: true }),
      rm(join(nodeTarget, "bin", "npx"), { force: true }),
    ]);
  } else {
    await Promise.all([
      rm(join(nodeTarget, "node_modules"), { recursive: true, force: true }),
      rm(join(nodeTarget, "npm"), { force: true }),
      rm(join(nodeTarget, "npm.cmd"), { force: true }),
      rm(join(nodeTarget, "npx"), { force: true }),
      rm(join(nodeTarget, "npx.cmd"), { force: true }),
    ]);
  }
  await writeFile(stamp, `${RUNTIME_ID}\n`);
  await rm(temp, { recursive: true, force: true });
}

async function prepareHarness() {
  await mkdir(harnessTarget, { recursive: true });
  const packageJson = JSON.parse(await readFile(join(harnessTarget, "package.json"), "utf8"));
  if (packageJson.dependencies?.["@deepseek-ai/dsh"] !== DSH_VERSION) {
    throw new Error(`Harness package.json must pin @deepseek-ai/dsh ${DSH_VERSION}`);
  }
  await run("pnpm", ["install", "--prod", "--ignore-scripts", "--frozen-lockfile", "--config.node-linker=hoisted"], harnessTarget);

  // The desktop WebView enters from tauri://localhost. A Strict cookie is
  // intentionally secure for normal browser handoff, but WebKit omits it on
  // the token URL's cross-site 303 redirect. Lax preserves the local-only
  // exchange while allowing the redirected root request to carry the cookie.
  const connectionFile = join(
    harnessTarget,
    "node_modules",
    "@deepseek-ai",
    "dsh-client-connection",
    "lib",
    "index.js",
  );
  const connectionSource = await readFile(connectionFile, "utf8");
  if (connectionSource.includes("SameSite=Strict")) {
    await writeFile(connectionFile, connectionSource.replace("SameSite=Strict", "SameSite=Lax"));
  }

  // Tauri follows bundled symlinks and rejects stale package-manager bin links.
  const binDir = join(harnessTarget, "node_modules", ".bin");
  for (const entry of await readdir(binDir, { withFileTypes: true })) {
    if (!entry.isSymbolicLink()) continue;
    try {
      await access(join(binDir, entry.name));
    } catch {
      await rm(join(binDir, entry.name), { force: true });
    }
  }
}

await Promise.all([prepareNode(), prepareHarness()]);
console.log(`Prepared Node ${NODE_VERSION} and DeepSeek Harness ${DSH_VERSION}.`);
