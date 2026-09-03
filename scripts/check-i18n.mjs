// Guards the two things that make a bilingual interface stay bilingual:
// the dictionaries hold the same keys, and no user-visible text is written
// anywhere else.
import { readFileSync, readdirSync } from "node:fs";
import { join } from "node:path";

const dictionary = readFileSync("src/i18n.ts", "utf8");
const problems = [];

// 1. Both languages say the same things.
const ruStart = dictionary.indexOf("const ru: Dict = {");
const enStart = dictionary.indexOf("const en: Dict = {");
const keysIn = (text) => new Set([...text.matchAll(/^ {2}"([\w.@-]+)":/gm)].map((m) => m[1]));
const ru = keysIn(dictionary.slice(ruStart, enStart));
const en = keysIn(dictionary.slice(enStart));

for (const key of ru) if (!en.has(key)) problems.push(`missing in en: ${key}`);
for (const key of en) if (!ru.has(key)) problems.push(`missing in ru: ${key}`);

// 2. Nothing outside the dictionary is written in one language.
//    The core names what it did; the wording lives here.
const cyrillic = /[А-Яа-яЁё]/;
for (const file of readdirSync("src").filter((f) => f.endsWith(".ts") && f !== "i18n.ts")) {
  const lines = readFileSync(join("src", file), "utf8").split("\n");
  lines.forEach((line, index) => {
    if (cyrillic.test(line)) problems.push(`${file}:${index + 1} holds text: ${line.trim()}`);
  });
}

// 3. The same for the markup, where a default is allowed only beside a key.
const html = readFileSync("index.html", "utf8").split("\n");
html.forEach((line, index) => {
  if (!cyrillic.test(line)) return;
  if (/data-i18n(-\w+)?=/.test(line)) return;
  problems.push(`index.html:${index + 1} holds untranslated text: ${line.trim()}`);
});

// 4. Every key the interface asks for has to exist. A missing one shows up as
//    the key itself in a tooltip, which is how `rail.deselect` shipped.
const known = new Set([...ru]);
const referenced = new Map();
const note = (key, where) => {
  if (!referenced.has(key)) referenced.set(key, where);
};

for (const [, key] of html.join("\n").matchAll(/data-i18n(?:-\w+)?="([\w.@-]+)"/g)) {
  note(key, "index.html");
}
for (const file of readdirSync("src").filter((f) => f.endsWith(".ts") && f !== "i18n.ts")) {
  const source = readFileSync(join("src", file), "utf8");
  // Literal keys only: a computed one cannot be checked from here.
  for (const [, key] of source.matchAll(/\bt\(\s*"([\w.@-]+)"/g)) note(key, file);
}

// The core names keys as well — errors it raises and steps it reports are
// rendered from the same dictionaries, and a missing one shows as a bare key.
const rustFiles = [];
const walk = (dir) => {
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const path = join(dir, entry.name);
    if (entry.isDirectory()) walk(path);
    else if (entry.name.endsWith(".rs")) rustFiles.push(path);
  }
};
walk(join("src-tauri", "src"));

for (const file of rustFiles) {
  const source = readFileSync(file, "utf8");
  for (const [, key] of source.matchAll(/AppError::message\(\s*"([\w.@-]+)"/g)) {
    note(key, file);
  }
  for (const [, key] of source.matchAll(/Phrase::new\(\s*"([\w.@-]+)"/g)) {
    if (key) note(key, file);
  }
  for (const [, key] of source.matchAll(/(?:key|reply_key):\s*"([\w.]+\.[\w.]+)"/g)) {
    note(key, file);
  }
}

for (const [key, where] of referenced) {
  if (!known.has(key)) problems.push(`${where} asks for a key nothing defines: ${key}`);
}

if (problems.length > 0) {
  console.error(problems.join("\n"));
  console.error(`\n${problems.length} problems`);
  process.exit(1);
}
console.log(
  `i18n ok: ${ru.size} keys in both languages, ${referenced.size} referenced, no stray text`,
);
