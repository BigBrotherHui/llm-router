// How does bili resolve the per-model output ceiling (models.<name>.output)?
import fs from 'fs';
const F = 'C:\\Users\\17277\\.dsh\\profiles\\tauri\\node_modules\\billion-context\\dist\\index.js';
const txt = fs.readFileSync(F, 'utf8');
const needles = ['outputCeiling', 'OutputCeiling', 'knownOutput', 'modelOutputLimit', 'outputLimit', 'models?.[', 'modelEntry', 'nativeWindowIsDeclared', 'declaredOutput'];
for (const n of needles) {
  let i = -1, c = 0;
  while ((i = txt.indexOf(n, i + 1)) !== -1 && c < 4) {
    c++;
    console.log(`\n### [${n}] @${i}\n${txt.slice(Math.max(0, i - 400), i + 400).replace(/\s+/g, ' ')}`);
  }
  if (c === 0) console.log(`\n### [${n}] NOT FOUND`);
}
