// Find files on disk that mention the synthetic window (1049000) or bili model-limit maps.
import fs from 'fs';
import path from 'path';

const roots = [
  'C:\\Users\\17277\\.dsh',
  'C:\\Users\\17277\\.bili',
  'C:\\Users\\17277\\AppData\\Roaming\\billion-context',
  'C:\\Users\\17277\\AppData\\Local\\billion-context',
  'D:\\Projects\\llm-router\\.llmrouter',
];
const needles = ['1049000', 'modelMaxOutputs', 'modelWindows', '1018356'];
const results = [];
let scanned = 0;

function walk(dir, depth, root) {
  let ents;
  try { ents = fs.readdirSync(dir, { withFileTypes: true }); } catch { return; }
  for (const e of ents) {
    const p = path.join(dir, e.name);
    if (e.isDirectory()) {
      if (depth > 8) continue;
      // skip heavy dirs that are not config holders
      if (/^(\.git|target|dist|build|\.next|__pycache__|node_modules)$/.test(e.name)) {
        if (e.name === 'node_modules') {
          // only descend into billion-context subtree
          const bc = path.join(p, 'billion-context');
          if (fs.existsSync(bc)) walk(bc, depth + 1, root);
        }
        continue;
      }
      walk(p, depth + 1, root);
    } else {
      if (!/\.(json|jsonl|yaml|yml|js|mjs|cjs|ts|md|txt|conf|log)$/i.test(e.name)) continue;
      let st; try { st = fs.statSync(p); } catch { continue; }
      if (st.size > 4 * 1024 * 1024) continue;
      scanned++;
      let txt; try { txt = fs.readFileSync(p, 'utf8'); } catch { continue; }
      for (const n of needles) {
        const i = txt.indexOf(n);
        if (i === -1) continue;
        results.push(`${p}  [${n}]@${i}  fileSize=${st.size} mtime=${st.mtime.toISOString()}\n    ...${txt.slice(Math.max(0, i - 220), i + 220).replace(/\s+/g, ' ')}...`);
      }
    }
  }
}
for (const r of roots) if (fs.existsSync(r)) walk(r, 0, r);
console.log('scanned files:', scanned, ' hits:', results.length);
console.log(results.slice(0, 30).join('\n\n'));
