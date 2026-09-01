// The on-device recogniser is loaded at runtime, not bundled: the ONNX
// Runtime package references several multi-megabyte .wasm builds, and letting
// the bundler follow those references drags ~90 MB of unused kernels into the
// installer. Copying just the two files we actually run keeps the app small
// and the webview offline-capable.
import { copyFileSync, mkdirSync, statSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");

const jobs = [
  // Self-contained ESM build; imported dynamically so Vite never touches it.
  {
    from: join(root, "node_modules", "@huggingface", "transformers", "dist", "transformers.web.js"),
    to: join(root, "public", "vendor", "transformers.web.js"),
  },
  // Single-threaded SIMD kernels — the build ORT picks without cross-origin
  // isolation, which a webview never has.
  {
    from: join(root, "node_modules", "onnxruntime-web", "dist", "ort-wasm-simd-threaded.mjs"),
    to: join(root, "public", "ort", "ort-wasm-simd-threaded.mjs"),
  },
  {
    from: join(root, "node_modules", "onnxruntime-web", "dist", "ort-wasm-simd-threaded.wasm"),
    to: join(root, "public", "ort", "ort-wasm-simd-threaded.wasm"),
  },
];

let total = 0;
for (const { from, to } of jobs) {
  mkdirSync(dirname(to), { recursive: true });
  copyFileSync(from, to);
  total += statSync(to).size;
}

console.log(`[copy-ort] ${jobs.length} files → public (${(total / 1048576).toFixed(1)} MB)`);
