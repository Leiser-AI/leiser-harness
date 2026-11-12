// Builds the installers: the harness service first, placed where Tauri
// bundles it next to the app (`externalBin` in tauri.release.conf.json),
// then the app itself.
//
//   npm run package                                   # this machine's system
//   npm run package -- --target universal-apple-darwin
//
// Further arguments go to `tauri build` (for example `--bundles dmg`).
import { spawnSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";

const desktop = path.resolve(import.meta.dirname, "..");
const workspace = path.resolve(desktop, "../..");
const SERVICE = "leiser-harness-service";
const UNIVERSAL = "universal-apple-darwin";

function run(program, args, options = {}) {
  const result = spawnSync(program, args, {
    stdio: "inherit",
    shell: process.platform === "win32",
    ...options,
  });
  if (result.status !== 0) {
    process.exit(result.status ?? 1);
  }
}

function hostTarget() {
  const result = spawnSync("rustc", ["-vV"], { encoding: "utf8", shell: process.platform === "win32" });
  const host = /^host: (.+)$/m.exec(result.stdout ?? "");
  if (!host) {
    console.error("could not read the host target from `rustc -vV`");
    process.exit(1);
  }
  return host[1].trim();
}

function buildService(target) {
  run("cargo", ["build", "--release", "--locked", "-p", SERVICE, "--target", target], { cwd: workspace });
  const suffix = target.includes("windows") ? ".exe" : "";
  return path.join(workspace, "target", target, "release", `${SERVICE}${suffix}`);
}

const args = process.argv.slice(2);
const targetIndex = args.indexOf("--target");
const target = targetIndex >= 0 ? args[targetIndex + 1] : hostTarget();

const binaries = path.join(desktop, "binaries");
fs.mkdirSync(binaries, { recursive: true });
const suffix = target.includes("windows") ? ".exe" : "";
const sidecar = path.join(binaries, `${SERVICE}-${target}${suffix}`);

if (target === UNIVERSAL) {
  // Tauri builds the app once per architecture, and each build wants the
  // service of its own architecture; the bundle takes the joined one.
  const architectures = ["aarch64-apple-darwin", "x86_64-apple-darwin"];
  const parts = architectures.map((architecture) => {
    const part = buildService(architecture);
    fs.copyFileSync(part, path.join(binaries, `${SERVICE}-${architecture}`));
    return part;
  });
  run("lipo", ["-create", "-output", sidecar, ...parts]);
} else {
  fs.copyFileSync(buildService(target), sidecar);
}

const tauriArgs = ["build", "--config", "tauri.release.conf.json"];
if (targetIndex < 0) {
  tauriArgs.push("--target", target);
}
run("npm", ["exec", "--", "tauri", ...tauriArgs, ...args], { cwd: desktop });
