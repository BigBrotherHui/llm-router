import fs from 'node:fs';
import zlib from 'node:zlib';

function unzstd(p) {
  const b = fs.readFileSync(p);
  const magic = Buffer.from([0x28, 0xb5, 0x2f, 0xfd]);
  const parts = [];
  let start = -1;
  for (let i = 0; i + 4 <= b.length; i++) {
    if (b[i] === 0x28 && b[i + 1] === 0xb5 && b[i + 2] === 0x2f && b[i + 3] === 0xfd) {
      if (start >= 0) parts.push(b.subarray(start, i));
      start = i;
    }
  }
  if (start >= 0) parts.push(b.subarray(start));
  const out = [];
  for (const p of parts) {
    try { out.push(zlib.zstdDecompressSync(p)); } catch (e) { /* skip bad frame */ }
  }
  return Buffer.concat(out).toString('utf8');
}

const src = process.argv[2];
const dst = process.argv[3];
const txt = unzstd(src);
fs.writeFileSync(dst, txt);
console.log('frames->bytes', txt.length, '=>', dst);
const recs = txt.split('\n').filter(Boolean).map((s) => { try { return JSON.parse(s); } catch { return null; } }).filter(Boolean);
const types = {};
recs.forEach((r) => { types[r.type] = (types[r.type] || 0) + 1; });
console.log('TYPES:', JSON.stringify(types));
for (const r of recs) {
  if (r.type === 'model/selection') console.log('model/selection:', JSON.stringify(r.data));
  if (r.type === 'request/header') console.log('request/header seq=' + r.seq + ' t=' + r.time + ' config=' + JSON.stringify(r.data.header.config) + ' adapterDefaults=' + JSON.stringify(r.data.header.adapterDefaults) + ' reason=' + r.data.reason);
  if (r.type === 'request/context') console.log('request/context:', JSON.stringify(r.data));
}
