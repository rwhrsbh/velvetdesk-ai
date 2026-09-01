// Generates the 1024x1024 source icon (app-icon.png) with zero dependencies.
// Run: node scripts/make-icon.mjs   then: npx tauri icon app-icon.png
import { deflateSync } from "node:zlib";
import { writeFileSync } from "node:fs";

const SIZE = 1024;

function lerp(a, b, t) {
  return a + (b - a) * t;
}

function mix(c1, c2, t) {
  return [lerp(c1[0], c2[0], t), lerp(c1[1], c2[1], t), lerp(c1[2], c2[2], t)];
}

// Rounded-square mask so the mark reads well on every platform.
function roundedMask(x, y, size, radius) {
  const inset = size * 0.045;
  const min = inset;
  const max = size - inset;
  if (x < min || y < min || x > max || y > max) return 0;
  const corners = [
    [min + radius, min + radius],
    [max - radius, min + radius],
    [min + radius, max - radius],
    [max - radius, max - radius],
  ];
  for (const [cx, cy] of corners) {
    const outsideX = (x < min + radius && cx === min + radius) || (x > max - radius && cx === max - radius);
    const outsideY = (y < min + radius && cy === min + radius) || (y > max - radius && cy === max - radius);
    if (outsideX && outsideY) {
      const d = Math.hypot(x - cx, y - cy);
      return d <= radius ? 1 : Math.max(0, 1 - (d - radius));
    }
  }
  return 1;
}

// Four-point sparkle, the VelvetDesk mark.
function sparkle(x, y, cx, cy, r) {
  const dx = Math.abs(x - cx) / r;
  const dy = Math.abs(y - cy) / r;
  const d = Math.pow(dx, 0.62) + Math.pow(dy, 0.62);
  return d <= 1 ? 1 : Math.max(0, 1 - (d - 1) * 12);
}

const raw = Buffer.alloc(SIZE * (SIZE * 4 + 1));
let p = 0;
for (let y = 0; y < SIZE; y++) {
  raw[p++] = 0; // filter type: none
  for (let x = 0; x < SIZE; x++) {
    const t = (x / SIZE) * 0.35 + (y / SIZE) * 0.65;
    let color = mix([88, 46, 168], [0, 122, 255], t);
    color = mix(color, [175, 82, 222], Math.pow(1 - y / SIZE, 2) * 0.5);

    const glow = sparkle(x, y, SIZE * 0.5, SIZE * 0.46, SIZE * 0.3);
    if (glow > 0) color = mix(color, [255, 255, 255], glow * 0.95);

    const small = sparkle(x, y, SIZE * 0.72, SIZE * 0.74, SIZE * 0.1);
    if (small > 0) color = mix(color, [255, 255, 255], small * 0.85);

    const alpha = roundedMask(x, y, SIZE, SIZE * 0.22);
    raw[p++] = Math.round(color[0]);
    raw[p++] = Math.round(color[1]);
    raw[p++] = Math.round(color[2]);
    raw[p++] = Math.round(alpha * 255);
  }
}

function crc32(buf) {
  let c;
  const table = [];
  for (let n = 0; n < 256; n++) {
    c = n;
    for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    table[n] = c >>> 0;
  }
  let crc = 0xffffffff;
  for (const byte of buf) crc = table[(crc ^ byte) & 0xff] ^ (crc >>> 8);
  return (crc ^ 0xffffffff) >>> 0;
}

function chunk(type, data) {
  const len = Buffer.alloc(4);
  len.writeUInt32BE(data.length);
  const body = Buffer.concat([Buffer.from(type, "ascii"), data]);
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(body));
  return Buffer.concat([len, body, crc]);
}

const ihdr = Buffer.alloc(13);
ihdr.writeUInt32BE(SIZE, 0);
ihdr.writeUInt32BE(SIZE, 4);
ihdr[8] = 8; // bit depth
ihdr[9] = 6; // RGBA
const png = Buffer.concat([
  Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
  chunk("IHDR", ihdr),
  chunk("IDAT", deflateSync(raw, { level: 9 })),
  chunk("IEND", Buffer.alloc(0)),
]);

writeFileSync(new URL("../app-icon.png", import.meta.url), png);
console.log("wrote app-icon.png", png.length, "bytes");
