import fs from 'node:fs';
import path from 'node:path';

const root = 'C:/Users/17277/AppData/Local/pnpm/store/v11/projects/cb213659c0244eec4ac4ac6206d844f0/node_modules/@deepseek-ai';
const hits = [];
let scanned = 0;
function walk(dir, depth) {
  if (depth > 5) return;
  let ents; try { ents = fs.readdirSync(dir, { withFileTypes: true }); } catch { return; }
  for (const e of ents) {
    const p = path.join(dir, e.name);
    if (e.isDirectory()) { walk(p, depth + 1); continue; }
    const ext = path.extname(e.name).toLowerCase();
    if (!['.js', '.cjs', '.mjs', '.json', '.ts', '.yaml', '.yml'].includes(ext)) continue;
    let st; try { st = fs.statSync(p); } catch { continue; }
    if (st.size > 40 * 1024 * 1024 || st.size < 100) continue;
    let text; try { text = fs.readFileSync(p, 'utf8'); } catch { continue; }
    scanned++;
    let i = text.indexOf('1049000');
    let count = 0;
    const ctxs = [];
    while (i >= 0 && count < 4) {
      ctxs.push(text.slice(Math.max(0, i - 260), i + 120).replace(/\s+/g, ' '));
      count++;
      i = text.indexOf('1049000', i + 1);
    }
    if (count > 0) hits.push({ p, size: st.size, count, ctxs });
  }
}
walk(root, 0);
console.log('scanned=' + scanned + ' files-with-1049000=' + hits.length);
for (const h of hits) {
  console.log('\n=== ' + h.p + ' size=' + h.size + ' n=' + h.count);
  for (const c of h.ctxs) console.log('   … ' + c);
}
