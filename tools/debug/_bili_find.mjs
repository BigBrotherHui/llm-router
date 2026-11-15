// Find who sets max_completion_tokens / the bili max-output headers, with context.
import fs from 'fs';
import path from 'path';

const ROOT = 'C:\\Users\\17277\\.dsh\\profiles\\tauri\\node_modules\\billion-context';
const patterns = [
  'max_completion_tokens',
  'x-bili-plugin-max-output',
  'x-bili-plugin-context-window',
  'OUTPUT_CLAMP_MIN_MARGIN',
  'OUTPUT_CLAMP_MARGIN_PCT',
  'modelMaxOutputs',
  'maxOutput',
];

function walk(dir, out = []) {
  for (const e of fs.readdirSync(dir, { withFileTypes: true })) {
    const p = path.join(dir, e.name);
    if (e.isDirectory()) walk(p, out);
    else if (/\.(js|mjs|cjs)$/.test(e.name)) out.push(p);
  }
  return out;
}

const files = walk(ROOT);
console.log('scanned files:', files.length);
for (const f of files) {
  const txt = fs.readFileSync(f, 'utf8');
  for (const pat of patterns) {
    let i = -1;
    let hits = 0;
    while ((i = txt.indexOf(pat, i + 1)) !== -1 && hits < 3) {
      hits++;
      const ctx = txt.slice(Math.max(0, i - 260), i + 160).replace(/\s+/g, ' ');
      console.log(`\n=== ${path.relative(ROOT, f)} @${i} [${pat}] hits#${hits}\n...${ctx}...`);
    }
  }
}
