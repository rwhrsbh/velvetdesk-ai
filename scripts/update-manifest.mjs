/**
 * Builds `latest.json` — the file the app reads to learn there is an update.
 *
 * The updater trusts a build only if it carries a signature made with the
 * project's private key, so the manifest pairs each platform's bundle with the
 * `.sig` the build produced next to it. Anything without a signature is left
 * out: an entry the app cannot verify is worse than no entry at all.
 *
 *   node scripts/update-manifest.mjs <artifacts dir> <version>
 */
import { readdirSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";

const [dir, version] = process.argv.slice(2);
if (!dir || !version) {
  console.error("usage: update-manifest.mjs <dir> <version>");
  process.exit(2);
}

const REPO = "rwhrsbh/velvetdesk-ai";
const files = readdirSync(dir);

/** Which platform key a bundle belongs to, as the updater names them. */
function platformOf(name) {
  if (name.endsWith(".msi") || name.endsWith(".exe")) return "windows-x86_64";
  if (name.endsWith(".AppImage")) return "linux-x86_64";
  if (name.endsWith(".app.tar.gz")) {
    return name.includes("aarch64") ? "darwin-aarch64" : "darwin-x86_64";
  }
  return null;
}

const platforms = {};
for (const name of files) {
  if (name.endsWith(".sig")) continue;
  const platform = platformOf(name);
  if (!platform || platforms[platform]) continue;

  const signature = `${name}.sig`;
  if (!files.includes(signature)) continue;

  platforms[platform] = {
    signature: readFileSync(join(dir, signature), "utf8").trim(),
    url: `https://github.com/${REPO}/releases/download/v${version}/${encodeURIComponent(name)}`,
  };
}

const manifest = {
  version,
  notes: `VelvetDesk AI v${version}`,
  pub_date: new Date().toISOString(),
  platforms,
};

const out = join(dir, "latest.json");
writeFileSync(out, `${JSON.stringify(manifest, null, 2)}\n`);
console.log(`[update-manifest] ${out}: ${Object.keys(platforms).join(", ") || "no signed bundles"}`);
