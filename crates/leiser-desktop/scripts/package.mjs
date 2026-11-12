// Builds the installers: the harness service first, placed where Tauri
// bundles it next to the app (`externalBin` in tauri.release.conf.json),
// then the app itself.
//
// A beta version (`X.Y.Z-beta.N` in tauri.conf.json) reads the beta update
// feed instead of the stable one. With TAURI_SIGNING_PRIVATE_KEY (and
// TAURI_SIGNING_PRIVATE_KEY_PASSWORD) set, the build also signs the
// installers the app updater installs; without, it builds no updates.
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
const binaries = path.join(desktop, "binaries");
const SERVICE = "leiser-harness-service";
const UNIVERSAL = "universal-apple-darwin";
const MAC_ARCHITECTURES = ["aarch64-apple-darwin", "x86_64-apple-darwin"];

function fail(message) {
  console.error(`package: ${message}`);
  process.exit(1);
}

function run(program, args, options = {}) {
  const result = spawnSync(program, args, {
    stdio: "inherit",
    shell: process.platform === "win32",
    ...options,
  });
  if (result.error) fail(`could not run ${program}: ${result.error.message}`);
  if (result.status !== 0) process.exit(result.status ?? 1);
}

function exeSuffix(target) {
  return target.includes("windows") ? ".exe" : "";
}

function hostTarget() {
  const result = spawnSync("rustc", ["-vV"], { encoding: "utf8", shell: process.platform === "win32" });
  const host = /^host: (.+)$/m.exec(result.stdout ?? "");
  if (!host) fail("could not read the host target from `rustc -vV`");
  return host[1].trim();
}

/** The target from `--target X` or `--target=X`, and the other arguments. */
function readTarget(args) {
  const rest = [];
  let target;
  for (let i = 0; i < args.length; i++) {
    if (args[i] === "--target") {
      target = args[++i];
      if (!target) fail("--target needs a value");
    } else if (args[i].startsWith("--target=")) {
      target = args[i].slice("--target=".length);
    } else {
      rest.push(args[i]);
    }
  }
  return { target: target ?? hostTarget(), rest };
}

/** Builds the service for `target` and copies it to `binaries/` under the
 * name Tauri looks for; returns the copy's path. */
function bundleService(target) {
  run("cargo", ["build", "--release", "--locked", "-p", SERVICE, "--target", target], { cwd: workspace });
  const suffix = exeSuffix(target);
  const sidecar = path.join(binaries, `${SERVICE}-${target}${suffix}`);
  fs.copyFileSync(path.join(workspace, "target", target, "release", `${SERVICE}${suffix}`), sidecar);
  return sidecar;
}

const { target, rest } = readTarget(process.argv.slice(2));
fs.mkdirSync(binaries, { recursive: true });

if (target === UNIVERSAL) {
  // Tauri builds the app once per architecture, and each build wants the
  // service of its own architecture; the bundle takes the joined one.
  const parts = MAC_ARCHITECTURES.map(bundleService);
  run("lipo", ["-create", "-output", path.join(binaries, `${SERVICE}-${UNIVERSAL}`), ...parts]);
} else {
  bundleService(target);
}

/** tauri.release.conf.json plus what this build adds: the beta feed for a
 * beta version, and update signatures when there is a key. */
function packageConfig() {
  const release = JSON.parse(fs.readFileSync(path.join(desktop, "tauri.release.conf.json"), "utf8"));
  const app = JSON.parse(fs.readFileSync(path.join(desktop, "tauri.conf.json"), "utf8"));
  const signs = Boolean(process.env.TAURI_SIGNING_PRIVATE_KEY);
  release.bundle.createUpdaterArtifacts = signs;
  if (app.version.includes("-beta.")) {
    // tauri.conf.json names the stable feed; the beta feed is next to it.
    const endpoints = app.plugins.updater.endpoints.map((url) => url.replace(/stable\.json$/, "beta.json"));
    release.plugins = { updater: { endpoints } };
    // RPM versions cannot hold the `-` of a beta: betas come as .deb only.
    release.bundle.targets = release.bundle.targets.filter((bundle) => bundle !== "rpm");
  }
  const config = path.join(binaries, "tauri.package.conf.json");
  fs.writeFileSync(config, JSON.stringify(release, null, 2));
  return { config, signs };
}

/** The Linux packages of `target` the app updater installs; the bundler
 * signs only the macOS and Windows ones. */
function linuxPackages(target) {
  const bundle = path.join(workspace, "target", target, "release", "bundle");
  const inFolder = (folder, ending) =>
    fs.existsSync(path.join(bundle, folder))
      ? fs
          .readdirSync(path.join(bundle, folder))
          .filter((name) => name.endsWith(ending))
          .map((name) => path.join(bundle, folder, name))
      : [];
  return [...inFolder("deb", ".deb"), ...inFolder("rpm", ".rpm")];
}

const { config, signs } = packageConfig();
run("npm", ["exec", "--", "tauri", "build", "--config", config, "--target", target, ...rest], { cwd: desktop });

if (signs) {
  for (const linuxPackage of linuxPackages(target)) {
    // A signature of an earlier build of the same version would not match.
    fs.rmSync(`${linuxPackage}.sig`, { force: true });
    run("npm", ["exec", "--", "tauri", "signer", "sign", linuxPackage], { cwd: desktop });
  }
}
