#!/usr/bin/env node
// Build the desktop's Windows and macOS icon containers from the committed
// PNGs (hub #3016, slice 14). Both formats can wrap PNG images as they are,
// so no image library or platform tool (iconutil, sips) is needed:
//
//   src-tauri/icons/icon.ico   32, 128 and 256 px PNG entries (Windows Vista+).
//                              tauri-build needs it to make the Windows
//                              resource file, so `cargo check --target
//                              x86_64-pc-windows-msvc` fails without it.
//   src-tauri/icons/icon.icns  icp5 (32), ic07 (128), ic08 (256), ic09 (512)
//                              PNG elements (macOS 10.7+), the .app icon.
//
//   node scripts/icons.mjs           write them
//   node scripts/icons.mjs --check   exit 1 when one is stale
//                                    (tests/icons.test.mjs runs the same check)
//
// Node built-ins only; runs the same on macOS, Linux and Windows.
import { readFileSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const desktopDir = resolve(dirname(fileURLToPath(import.meta.url)), '..');
export const ICON_DIR = join(desktopDir, 'src-tauri', 'icons');

/** Source PNG per edge length. */
export const SOURCES = { 32: '32x32.png', 128: '128x128.png', 256: '256x256.png', 512: 'icon.png' };
const ICO_SIZES = [32, 128, 256];
const ICNS_TYPES = [
  [32, 'icp5'],
  [128, 'ic07'],
  [256, 'ic08'],
  [512, 'ic09'],
];

const PNG_SIGNATURE = Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]);

/** Width and height from a PNG's IHDR chunk; throws on anything else. */
export function pngSize(png) {
  if (png.length < 24 || !png.subarray(0, 8).equals(PNG_SIGNATURE) || png.toString('latin1', 12, 16) !== 'IHDR') {
    throw new Error('not a PNG file');
  }
  return { width: png.readUInt32BE(16), height: png.readUInt32BE(20) };
}

function checked(pngs, size) {
  const png = pngs[size];
  if (!png) throw new Error(`missing the ${size} px PNG`);
  const { width, height } = pngSize(png);
  if (width !== size || height !== size) {
    throw new Error(`the ${size} px PNG is ${width}x${height}`);
  }
  return png;
}

/** An ICO file whose entries are the given PNGs (`{size: Buffer}`). */
export function buildIco(pngs, sizes = ICO_SIZES) {
  const images = sizes.map((size) => checked(pngs, size));
  const header = Buffer.alloc(6);
  header.writeUInt16LE(0, 0); // reserved
  header.writeUInt16LE(1, 2); // type: icon
  header.writeUInt16LE(images.length, 4);
  let offset = 6 + 16 * images.length;
  const entries = images.map((png, i) => {
    const size = sizes[i];
    if (size > 256) throw new Error('an ICO entry is at most 256 px');
    const entry = Buffer.alloc(16);
    entry.writeUInt8(size === 256 ? 0 : size, 0); // width (0 means 256)
    entry.writeUInt8(size === 256 ? 0 : size, 1); // height
    entry.writeUInt8(0, 2); // palette colours
    entry.writeUInt8(0, 3); // reserved
    entry.writeUInt16LE(1, 4); // colour planes
    entry.writeUInt16LE(32, 6); // bits per pixel
    entry.writeUInt32LE(png.length, 8);
    entry.writeUInt32LE(offset, 12);
    offset += png.length;
    return entry;
  });
  return Buffer.concat([header, ...entries, ...images]);
}

/** An ICNS file whose elements are the given PNGs (`{size: Buffer}`). */
export function buildIcns(pngs, types = ICNS_TYPES) {
  const elements = types.map(([size, type]) => {
    const png = checked(pngs, size);
    const head = Buffer.alloc(8);
    head.write(type, 0, 'latin1');
    head.writeUInt32BE(png.length + 8, 4);
    return Buffer.concat([head, png]);
  });
  const body = Buffer.concat(elements);
  const head = Buffer.alloc(8);
  head.write('icns', 0, 'latin1');
  head.writeUInt32BE(body.length + 8, 4);
  return Buffer.concat([head, body]);
}

export function readSources(dir = ICON_DIR) {
  const pngs = {};
  for (const [size, file] of Object.entries(SOURCES)) pngs[size] = readFileSync(join(dir, file));
  return pngs;
}

/** `[file, expected bytes]` for every generated icon. */
export function generated(dir = ICON_DIR) {
  const pngs = readSources(dir);
  return [
    ['icon.ico', buildIco(pngs)],
    ['icon.icns', buildIcns(pngs)],
  ];
}

/** The generated icons whose committed bytes differ from the sources. */
export function staleIcons(dir = ICON_DIR) {
  return generated(dir)
    .filter(([file, bytes]) => {
      try {
        return !readFileSync(join(dir, file)).equals(bytes);
      } catch {
        return true;
      }
    })
    .map(([file]) => file);
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  if (process.argv.includes('--check')) {
    const stale = staleIcons();
    if (stale.length > 0) {
      console.error(`stale: ${stale.join(', ')}; run \`node scripts/icons.mjs\``);
      process.exit(1);
    }
  } else {
    for (const [file, bytes] of generated()) writeFileSync(join(ICON_DIR, file), bytes);
  }
}
