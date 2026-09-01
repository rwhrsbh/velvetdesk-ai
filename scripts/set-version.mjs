// Stamps one version across the three files that carry it.
// Used by CI to build a fresh version on every push without committing a bump
// (a commit would re-trigger the workflow and loop forever).
//
//   node scripts/set-version.mjs 0.2.41
import { readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const version = process.argv[2];
if (!version || !/^\d+\.\d+\.\d+$/.test(version)) {
  console.error("usage: node scripts/set-version.mjs <major.minor.patch>");
  process.exit(1);
}

const root = join(dirname(fileURLToPath(import.meta.url)), "..");

function edit(relative, replacer) {
  const path = join(root, relative);
  const before = readFileSync(path, "utf8");
  const after = replacer(before);
  if (before === after) {
    console.error(`[set-version] nothing replaced in ${relative}`);
    process.exit(1);
  }
  writeFileSync(path, after);
  console.log(`[set-version] ${relative} → ${version}`);
}

edit("package.json", (text) =>
  text.replace(/("version":\s*")\d+\.\d+\.\d+(")/, `$1${version}$2`),
);

edit("src-tauri/tauri.conf.json", (text) =>
  text.replace(/("version":\s*")\d+\.\d+\.\d+(")/, `$1${version}$2`),
);

// Only the [package] version, never a dependency's.
edit("src-tauri/Cargo.toml", (text) =>
  text.replace(/(\[package\][\s\S]*?\nversion\s*=\s*")\d+\.\d+\.\d+(")/, `$1${version}$2`),
);
