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

if (problems.length > 0) {
  console.error(problems.join("\n"));
  console.error(`\n${problems.length} problems`);
  process.exit(1);
}
console.log(`i18n ok: ${ru.size} keys in both languages, no stray text`);
