/**
 * A minimal PNG reader, just enough to answer one question honestly: did anything render?
 *
 * This exists because "the screenshot file was written" is not evidence that a window drew
 * anything — an all-black pane, a blank white error page and a real UI all produce a PNG of
 * a few kilobytes. So the pixels are decoded here (zlib inflate plus the five PNG row
 * filters; Chromium writes 8-bit non-interlaced RGB or RGBA, and anything else is refused
 * rather than guessed at) and reduced to numbers the shot script can assert on:
 *
 *   uniqueColors      — a uniform image has 1. A rendered UI has thousands (antialiased
 *                       text alone guarantees it).
 *   modalShare        — the fraction of pixels covered by the single most common colour.
 *                       A blank pane is ~1.0; a dark UI with one background colour is well
 *                       under it because of text, borders and the video frame.
 *   meanLuminance     — 0..1. The window is dark-mode only, so a real shot of it must be
 *                       dark; a white error page is not.
 *   transparentPixels — must be 0: a fully transparent PNG would pass a colour count and
 *                       still be invisible.
 *
 * No dependency is used or wanted here: the whole decoder is the forty lines below, and it
 * is exercised (indeed, only exists) through `shots.mjs`.
 */

import { readFileSync } from 'node:fs';
import { inflateSync } from 'node:zlib';

const PNG_MAGIC = 0x89504e47;
const CHANNELS = { 2: 3, 6: 4 };

/** Decode one PNG file into pixel statistics. Throws if it is not one we can read. */
export function pngStats(path) {
  const buf = readFileSync(path);
  if (buf.length < 8 || buf.readUInt32BE(0) !== PNG_MAGIC) {
    throw new Error(`${path}: not a PNG (bad magic)`);
  }

  let offset = 8;
  let width = 0;
  let height = 0;
  let bitDepth = 0;
  let colorType = 0;
  let interlace = 0;
  const idat = [];

  while (offset + 8 <= buf.length) {
    const length = buf.readUInt32BE(offset);
    const type = buf.toString('latin1', offset + 4, offset + 8);
    const start = offset + 8;
    const end = start + length;
    if (end > buf.length) throw new Error(`${path}: truncated ${type} chunk`);

    if (type === 'IHDR') {
      width = buf.readUInt32BE(start);
      height = buf.readUInt32BE(start + 4);
      bitDepth = buf[start + 8];
      colorType = buf[start + 9];
      interlace = buf[start + 12];
    } else if (type === 'IDAT') {
      idat.push(buf.subarray(start, end));
    } else if (type === 'IEND') {
      break;
    }
    offset = end + 4; // skip the CRC
  }

  const channels = CHANNELS[colorType];
  if (channels === undefined || bitDepth !== 8 || interlace !== 0) {
    throw new Error(
      `${path}: unsupported PNG (bitDepth=${bitDepth} colorType=${colorType} ` +
        `interlace=${interlace}); this decoder handles 8-bit RGB/RGBA only`,
    );
  }

  const stride = width * channels;
  const pixels = new Uint8Array(height * stride);
  const raw = inflateSync(Buffer.concat(idat));

  let previous = new Uint8Array(stride);
  for (let y = 0; y < height; y += 1) {
    const rowStart = y * (stride + 1);
    const filter = raw[rowStart];
    const row = raw.subarray(rowStart + 1, rowStart + 1 + stride);
    const current = pixels.subarray(y * stride, (y + 1) * stride);

    for (let x = 0; x < stride; x += 1) {
      const left = x >= channels ? current[x - channels] : 0;
      const above = previous[x];
      const upLeft = x >= channels ? previous[x - channels] : 0;
      const value = row[x];

      switch (filter) {
        case 0:
          current[x] = value;
          break;
        case 1:
          current[x] = (value + left) & 0xff;
          break;
        case 2:
          current[x] = (value + above) & 0xff;
          break;
        case 3:
          current[x] = (value + ((left + above) >> 1)) & 0xff;
          break;
        case 4: {
          const p = left + above - upLeft;
          const pa = Math.abs(p - left);
          const pb = Math.abs(p - above);
          const pc = Math.abs(p - upLeft);
          const pred = pa <= pb && pa <= pc ? left : pb <= pc ? above : upLeft;
          current[x] = (value + pred) & 0xff;
          break;
        }
        default:
          throw new Error(`${path}: unknown PNG row filter ${filter} on row ${y}`);
      }
    }
    previous = current;
  }

  // Pack each pixel as 0xRRGGBB, then sort once: distinct colours are then a single pass,
  // and the longest run of equal values is the modal share.
  const packed = new Uint32Array(width * height);
  let luminance = 0;
  let transparent = 0;

  for (let i = 0, p = 0; i < packed.length; i += 1, p += channels) {
    const r = pixels[p];
    const g = pixels[p + 1];
    const b = pixels[p + 2];
    packed[i] = (r << 16) | (g << 8) | b;
    luminance += (0.2126 * r + 0.7152 * g + 0.0722 * b) / 255;
    if (channels === 4 && pixels[p + 3] < 255) transparent += 1;
  }
  packed.sort();

  let uniqueColors = 1;
  let run = 1;
  let modalPixels = 1;
  let modalColor = packed[0];
  for (let i = 1; i < packed.length; i += 1) {
    if (packed[i] === packed[i - 1]) {
      run += 1;
    } else {
      uniqueColors += 1;
      if (run > modalPixels) {
        modalPixels = run;
        modalColor = packed[i - 1];
      }
      run = 1;
    }
  }
  if (run > modalPixels) {
    modalPixels = run;
    modalColor = packed[packed.length - 1];
  }

  const hex = `#${modalColor.toString(16).padStart(6, '0')}`;
  return {
    path,
    width,
    height,
    bytes: buf.length,
    uniqueColors,
    modalShare: modalPixels / packed.length,
    modalColor: hex,
    transparentPixels: transparent,
    meanLuminance: luminance / packed.length,
  };
}
