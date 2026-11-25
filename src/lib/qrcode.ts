// Minimal, zero-dependency QR Code SVG generator (Byte mode, ECC Level M,
// Versions 1–6) for rendering local LAN URLs in Settings.

const EC_CODEWORDS_M: Record<number, { total: number; ec: number; blocks: number }> = {
  1: { total: 26, ec: 10, blocks: 1 },
  2: { total: 44, ec: 16, blocks: 1 },
  3: { total: 70, ec: 26, blocks: 1 },
  4: { total: 100, ec: 36, blocks: 2 },
  5: { total: 134, ec: 48, blocks: 2 },
  6: { total: 172, ec: 64, blocks: 4 },
};

const ALIGNMENT_CENTERS: Record<number, number[]> = {
  1: [],
  2: [6, 18],
  3: [6, 22],
  4: [6, 26],
  5: [6, 30],
  6: [6, 34],
};

// GF(256) with primitive polynomial 0x11d
const GF_EXP = new Uint8Array(512);
const GF_LOG = new Uint8Array(256);
{
  let x = 1;
  for (let i = 0; i < 255; i++) {
    GF_EXP[i] = x;
    GF_LOG[x] = i;
    x <<= 1;
    if (x & 0x100) x ^= 0x11d;
  }
  for (let i = 255; i < 512; i++) GF_EXP[i] = GF_EXP[i - 255];
}

function gfMul(a: number, b: number): number {
  if (a === 0 || b === 0) return 0;
  return GF_EXP[GF_LOG[a] + GF_LOG[b]];
}

function rsGeneratorPoly(degree: number): Uint8Array {
  let poly = new Uint8Array([1]);
  for (let i = 0; i < degree; i++) {
    const next = new Uint8Array(poly.length + 1);
    const factor = GF_EXP[i];
    for (let j = 0; j < poly.length; j++) {
      next[j] ^= poly[j];
      next[j + 1] ^= gfMul(poly[j], factor);
    }
    poly = next;
  }
  return poly;
}

function rsEncode(data: Uint8Array, ecCount: number): Uint8Array {
  const gen = rsGeneratorPoly(ecCount);
  const res = new Uint8Array(data.length + ecCount);
  res.set(data);
  for (let i = 0; i < data.length; i++) {
    const coef = res[i];
    if (coef !== 0) {
      for (let j = 0; j < gen.length; j++) {
        res[i + j] ^= gfMul(gen[j], coef);
      }
    }
  }
  return res.slice(data.length);
}

function pickVersion(byteLen: number): number {
  for (let v = 1; v <= 6; v++) {
    const { total, ec } = EC_CODEWORDS_M[v];
    const dataBytes = total - ec;
    const capacity = dataBytes - 2; // 4-bit mode + 8-bit count + 4-bit terminator = 2 bytes
    if (byteLen <= capacity) return v;
  }
  throw new Error("QR payload too long");
}

function encodeData(bytes: Uint8Array, version: number): Uint8Array {
  const { total, ec, blocks } = EC_CODEWORDS_M[version];
  const dataBytes = total - ec;
  const bits: number[] = [];
  const pushBits = (val: number, len: number) => {
    for (let i = len - 1; i >= 0; i--) bits.push((val >> i) & 1);
  };

  // Mode: Byte (0100), Length: 8 bits (for versions 1-9)
  pushBits(0b0100, 4);
  pushBits(bytes.length, 8);
  for (const b of bytes) pushBits(b, 8);

  const maxBits = dataBytes * 8;
  pushBits(0, Math.min(4, maxBits - bits.length));
  while (bits.length % 8 !== 0) bits.push(0);

  const codewords: number[] = [];
  for (let i = 0; i < bits.length; i += 8) {
    let v = 0;
    for (let j = 0; j < 8; j++) v = (v << 1) | bits[i + j];
    codewords.push(v);
  }
  let padIdx = 0;
  while (codewords.length < dataBytes) {
    codewords.push(padIdx % 2 === 0 ? 0xec : 0x11);
    padIdx++;
  }

  // Split into blocks & compute RS EC
  const ecPerBlock = ec / blocks;
  const baseLen = Math.floor(dataBytes / blocks);
  const extra = dataBytes % blocks;
  const dataBlocks: Uint8Array[] = [];
  const ecBlocks: Uint8Array[] = [];
  let offset = 0;
  for (let b = 0; b < blocks; b++) {
    const len = baseLen + (b >= blocks - extra ? 1 : 0);
    const slice = new Uint8Array(codewords.slice(offset, offset + len));
    offset += len;
    dataBlocks.push(slice);
    ecBlocks.push(rsEncode(slice, ecPerBlock));
  }

  const out: number[] = [];
  const maxDataLen = baseLen + (extra > 0 ? 1 : 0);
  for (let i = 0; i < maxDataLen; i++) {
    for (const blk of dataBlocks) if (i < blk.length) out.push(blk[i]);
  }
  for (let i = 0; i < ecPerBlock; i++) {
    for (const blk of ecBlocks) out.push(blk[i]);
  }
  return new Uint8Array(out);
}

function buildMatrix(text: string): boolean[][] {
  const bytes = new TextEncoder().encode(text);
  const version = pickVersion(bytes.length);
  const size = version * 4 + 17;
  const modules: (boolean | null)[][] = Array.from({ length: size }, () =>
    Array(size).fill(null),
  );
  const reserved: boolean[][] = Array.from({ length: size }, () =>
    Array(size).fill(false),
  );

  const setFixed = (r: number, c: number, val: boolean) => {
    modules[r][c] = val;
    reserved[r][c] = true;
  };

  const placeFinder = (r0: number, c0: number) => {
    for (let dr = -1; dr <= 7; dr++) {
      for (let dc = -1; dc <= 7; dc++) {
        const r = r0 + dr;
        const c = c0 + dc;
        if (r < 0 || r >= size || c < 0 || c >= size) continue;
        const on =
          dr >= 0 &&
          dr <= 6 &&
          dc >= 0 &&
          dc <= 6 &&
          (dr === 0 || dr === 6 || dc === 0 || dc === 6 || (dr >= 2 && dr <= 4 && dc >= 2 && dc <= 4));
        setFixed(r, c, on);
      }
    }
  };

  placeFinder(0, 0);
  placeFinder(0, size - 7);
  placeFinder(size - 7, 0);

  // Timing patterns
  for (let i = 8; i < size - 8; i++) {
    const val = i % 2 === 0;
    if (!reserved[6][i]) setFixed(6, i, val);
    if (!reserved[i][6]) setFixed(i, 6, val);
  }

  // Alignment patterns
  const centers = ALIGNMENT_CENTERS[version];
  for (const r0 of centers) {
    for (const c0 of centers) {
      if (reserved[r0][c0]) continue;
      for (let dr = -2; dr <= 2; dr++) {
        for (let dc = -2; dc <= 2; dc++) {
          const on = Math.max(Math.abs(dr), Math.abs(dc)) !== 1;
          setFixed(r0 + dr, c0 + dc, on);
        }
      }
    }
  }

  // Reserve format info areas + dark module
  setFixed(size - 8, 8, true);
  for (let i = 0; i < 9; i++) {
    if (!reserved[8][i]) reserved[8][i] = true;
    if (!reserved[i][8]) reserved[i][8] = true;
  }
  for (let i = 0; i < 8; i++) {
    reserved[8][size - 1 - i] = true;
    reserved[size - 1 - i][8] = true;
  }

  // Place data bits (mask 0: (r + c) % 2 === 0)
  const data = encodeData(bytes, version);
  let bitIdx = 0;
  let dir = -1;
  for (let col = size - 1; col > 0; col -= 2) {
    if (col === 6) col--;
    for (let i = 0; i < size; i++) {
      const r = dir === -1 ? size - 1 - i : i;
      for (const dc of [0, -1]) {
        const c = col + dc;
        if (reserved[r][c]) continue;
        const byteI = bitIdx >> 3;
        const bit = byteI < data.length ? (data[byteI] >> (7 - (bitIdx & 7))) & 1 : 0;
        bitIdx++;
        const masked = (r + c) % 2 === 0 ? bit ^ 1 : bit;
        modules[r][c] = masked === 1;
      }
    }
    dir = -dir;
  }

  // Format info for ECC Level M (00) + Mask 0 (000) -> 0x5412 (MSB first).
  const formatBits = 0x5412;
  for (let i = 0; i < 15; i++) {
    const bit = ((formatBits >> (14 - i)) & 1) === 1;
    // Around top-left finder
    if (i < 6) modules[8][i] = bit;
    else if (i < 8) modules[8][i + 1] = bit;
    else modules[14 - i][8] = bit;
    // Around other two finders
    if (i < 8) modules[size - 1 - i][8] = bit;
    else modules[8][size - 15 + i] = bit;
  }

  return modules.map((row) => row.map((v) => v === true));
}

/** Generate a crisp SVG data URI for the given text (URL) with standard 4-module quiet zone. */
export function qrSvgDataUrl(text: string, sizePx = 168): string {
  const m = buildMatrix(text);
  const n = m.length;
  const quiet = 4;
  const total = n + quiet * 2;
  let path = "";
  for (let r = 0; r < n; r++) {
    for (let c = 0; c < n; c++) {
      if (m[r][c]) {
        path += `M${c + quiet},${r + quiet}h1v1h-1z`;
      }
    }
  }
  const svg = `<svg xmlns="http://www.w3.org/2000/svg" width="${sizePx}" height="${sizePx}" viewBox="0 0 ${total} ${total}" shape-rendering="crispEdges"><rect width="${total}" height="${total}" fill="#ffffff" rx="2"/><path d="${path}" fill="#111111"/></svg>`;
  return `data:image/svg+xml;charset=utf-8,${encodeURIComponent(svg)}`;
}
