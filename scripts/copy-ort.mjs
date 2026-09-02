// Builds the on-device recogniser as a standalone ESM file, loaded at runtime.
//
// It is kept out of the app bundle on purpose: ONNX Runtime references four
// multi-megabyte .wasm builds, and letting the app's bundler follow those
// references inflates the installer by ~90 MB.
//
// transformers.web.js is *not* self-contained — it keeps bare imports
// (`onnxruntime-web/webgpu`, which in turn imports `onnxruntime-common`), and a
// webview resolves no bare specifiers. esbuild resolves them here, so the
// webview loads one file with no import map and no CDN.
import { copyFileSync, mkdirSync, statSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { build } from "esbuild";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const ortDist = join(root, "node_modules", "onnxruntime-web", "dist");
const vendor = join(root, "public", "vendor");
const ortPublic = join(root, "public", "ort");

// WebGPU is aliased away deliberately: support across WebView2, WKWebView and
// Android WebView is inconsistent, and its kernels are a 26 MB binary against
// 13 MB for the wasm ones. `local-whisper.ts` pins `device: "wasm"` to match.
await build({
  entryPoints: [
    join(root, "node_modules", "@huggingface", "transformers", "dist", "transformers.web.js"),
  ],
  outfile: join(vendor, "transformers.web.js"),
  bundle: true,
  format: "esm",
  platform: "browser",
  alias: { "onnxruntime-web/webgpu": "onnxruntime-web/wasm" },
  legalComments: "none",
  logLevel: "warning",
});

// The kernels ORT loads at runtime from `wasmPaths`: single-threaded SIMD,
// the build it picks without cross-origin isolation, which a webview never
// has. The `jsep` pair belongs to the WebGPU backend and is deliberately left
// out — it is 26 MB the app would never execute.
const RUNTIME_FILES = ["ort-wasm-simd-threaded.mjs", "ort-wasm-simd-threaded.wasm"];

const jobs = RUNTIME_FILES.map((file) => ({
  from: join(ortDist, file),
  to: join(ortPublic, file),
}));

let total = statSync(join(vendor, "transformers.web.js")).size;
for (const { from, to } of jobs) {
  mkdirSync(dirname(to), { recursive: true });
  copyFileSync(from, to);
  total += statSync(to).size;
}

console.log(`[copy-ort] ${jobs.length + 1} files → public (${(total / 1048576).toFixed(1)} MB)`);
