/**
 * On-device speech recognition.
 *
 * Weights are downloaded by the Rust side into the app data directory and
 * served over the private `vdmodels` scheme, so this module never touches the
 * network: it only points transformers.js at that scheme and runs the model
 * through ONNX Runtime inside the webview.
 */
import { api } from "./api";

/** Minimal surface of the transformers.js build we load at runtime. */
interface Recogniser {
  (
    audio: Float32Array,
    options: Record<string, unknown>,
  ): Promise<{ text?: string } | Array<{ text?: string }>>;
}

interface TransformersModule {
  env: Record<string, any>;
  pipeline: (task: string, model: string, options?: Record<string, unknown>) => Promise<Recogniser>;
}

let runtime: TransformersModule | null = null;
let configured = false;
let loaded: { repo: string; recogniser: Recogniser } | null = null;
let loading: Promise<Recogniser> | null = null;

/**
 * Loaded from `public/vendor` instead of being imported normally: the package
 * points at four different multi-megabyte WASM builds, and a bundler that
 * follows those references inflates the installer by ~90 MB.
 */
async function loadRuntime(): Promise<TransformersModule> {
  if (runtime) return runtime;
  // Resolved against the document rather than written as a literal: a literal
  // path is still statically analysable, and the dev server rewrites it to
  // `/vendor/transformers.web.js?import`, which it then fails to serve
  // ("Failed to fetch dynamically imported module").
  const url = new URL("vendor/transformers.web.js", document.baseURI).href;
  runtime = (await import(/* @vite-ignore */ url)) as unknown as TransformersModule;
  return runtime;
}

/** Whisper wants 16 kHz mono float samples. */
const TARGET_RATE = 16_000;

async function configure(module: TransformersModule) {
  if (configured) return;
  const base = await api.localModelsBaseUrl();
  const { env } = module;

  env.allowLocalModels = false;
  env.allowRemoteModels = true;
  // Weights live under <base>/<repo>/<file> — the same layout as the Hub, but
  // served from disk by the Rust side.
  env.remoteHost = base.endsWith("/") ? base : `${base}/`;
  env.remotePathTemplate = "{model}/";
  env.useBrowserCache = false;

  const wasm = env.backends?.onnx?.wasm;
  if (wasm) {
    wasm.wasmPaths = "/ort/";
    // A webview is never cross-origin isolated, so threads and
    // SharedArrayBuffer are unavailable; single-threaded SIMD still runs
    // tiny and base comfortably.
    wasm.numThreads = 1;
  }
  configured = true;
}

/** Load (once) the recogniser for a downloaded model repository. */
export async function loadModel(repo: string): Promise<Recogniser> {
  if (loaded?.repo === repo) return loaded.recogniser;
  if (loading) await loading.catch(() => undefined);
  if (loaded?.repo === repo) return loaded.recogniser;

  const module = await loadRuntime();
  await configure(module);
  // `device` is pinned: the vendored ONNX Runtime is the wasm-only build, so
  // letting transformers.js auto-select WebGPU (navigator.gpu exists in some
  // webviews) would ask for a backend that is not linked in.
  loading = module.pipeline("automatic-speech-recognition", repo, {
    // `q4`, not `q8`: every 8-bit export of these models fails to build a
    // session in this ONNX Runtime (see WEIGHT_FILES in whisper.rs).
    dtype: "q4",
    device: "wasm",
  });
  try {
    const recogniser = await loading;
    loaded = { repo, recogniser };
    return recogniser;
  } finally {
    loading = null;
  }
}

export function unloadModel() {
  loaded = null;
}

/** Decode a recorded clip and resample it to what Whisper expects. */
export async function decodeAudio(blob: Blob): Promise<Float32Array> {
  const bytes = await blob.arrayBuffer();
  const AudioContextCtor =
    window.AudioContext ?? (window as unknown as { webkitAudioContext: typeof AudioContext }).webkitAudioContext;
  const context = new AudioContextCtor({ sampleRate: TARGET_RATE });
  try {
    const decoded = await context.decodeAudioData(bytes);
    if (decoded.numberOfChannels === 1) {
      return decoded.getChannelData(0).slice();
    }
    // Downmix to mono.
    const left = decoded.getChannelData(0);
    const right = decoded.getChannelData(1);
    const mono = new Float32Array(left.length);
    for (let i = 0; i < left.length; i += 1) {
      mono[i] = (left[i] + right[i]) / 2;
    }
    return mono;
  } finally {
    void context.close();
  }
}

export interface LocalTranscribeOptions {
  /** "ru", "uk", "en". Whisper falls back to English — and translates — when
   *  nothing is given, so callers should always pass one. */
  language?: string;
}

/** Root-mean-square level of a clip, 0 … 1. */
export function loudness(samples: Float32Array): number {
  if (samples.length === 0) return 0;
  let sum = 0;
  for (let i = 0; i < samples.length; i += 1) sum += samples[i] * samples[i];
  return Math.sqrt(sum / samples.length);
}

/** Below this a clip carries no speech; Whisper answers silence with
 *  hallucinations like "You" or "Thank you." rather than an empty string. */
export const SILENCE_RMS = 0.004;

export class SilentClipError extends Error {
  constructor() {
    super("silent clip");
    this.name = "SilentClipError";
  }
}

/** Transcribe a recorded clip entirely on this device. */
export async function transcribeLocally(
  repo: string,
  blob: Blob,
  options: LocalTranscribeOptions = {},
): Promise<string> {
  const recogniser = await loadModel(repo);
  const samples = await decodeAudio(blob);

  // A silent clip is worth reporting as such: feeding it to Whisper produces
  // confident nonsense instead of nothing.
  if (loudness(samples) < SILENCE_RMS) throw new SilentClipError();

  const output = await recogniser(samples, {
    // Long clips are processed in chunks with overlap so nothing is lost.
    chunk_length_s: 30,
    stride_length_s: 5,
    language: options.language || undefined,
    // Always transcribe: "translate" would turn dictated Russian into English.
    task: "transcribe",
  });

  const text = Array.isArray(output)
    ? output.map((part) => part.text ?? "").join(" ")
    : (output.text ?? "");
  return text.trim();
}
