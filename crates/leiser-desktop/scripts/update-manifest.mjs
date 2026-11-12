// Writes a release's update manifest (`latest.json`): for every platform,
// the installer the app updater downloads from the release and its
// signature. The update feed serves it once the release is published (see
// .github/workflows/update-feed.yml).
//
//   node update-manifest.mjs <installers dir> <tag> <owner/repo> <out file>
import fs from "node:fs";
import path from "node:path";

const [dir, tag, repository, out] = process.argv.slice(2);
if (!out) {
  console.error("usage: update-manifest.mjs <installers dir> <tag> <owner/repo> <out file>");
  process.exit(1);
}

const files = fs.readdirSync(dir, { recursive: true }).map((file) => path.join(dir, String(file)));

const beta = tag.includes("-beta.");

/** The one installer whose name ends in `ending`, with its signature;
 * `null` for one a beta does not have when `optionalForBeta`. */
function installer(ending, optionalForBeta = false) {
  const found = files.filter((file) => file.endsWith(ending));
  if (found.length === 0 && beta && optionalForBeta) return null;
  if (found.length !== 1) {
    console.error(`expected one installer ending in ${ending}, found ${found.length}`);
    process.exit(1);
  }
  const [file] = found;
  const signature = `${file}.sig`;
  if (!fs.existsSync(signature)) {
    console.error(`${file} has no signature`);
    process.exit(1);
  }
  return {
    url: `https://github.com/${repository}/releases/download/${tag}/${encodeURIComponent(path.basename(file))}`,
    signature: fs.readFileSync(signature, "utf8").trim(),
  };
}

// The universal macOS app serves both architectures.
const mac = installer(".app.tar.gz");
const manifest = {
  version: tag.replace(/^v/, ""),
  notes: `See https://github.com/${repository}/releases/tag/${tag}`,
  pub_date: new Date().toISOString(),
  platforms: {
    "darwin-aarch64": mac,
    "darwin-x86_64": mac,
    "windows-x86_64-nsis": installer("-setup.exe"),
    "linux-x86_64-deb": installer(".deb"),
  },
};
// RPM versions cannot hold the `-` of a beta, so betas have no .rpm.
const rpm = installer(".rpm", true);
if (rpm !== null) manifest.platforms["linux-x86_64-rpm"] = rpm;
fs.writeFileSync(out, `${JSON.stringify(manifest, null, 2)}\n`);
console.log(`wrote ${out} for ${tag}`);
