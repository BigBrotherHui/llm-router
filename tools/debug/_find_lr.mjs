import fs from 'node:fs';
import path from 'node:path';

const root = 'C:/Users/17277/.dsh';
const skipDirs = new Set(['node_modules', 'sessions', 'session_projcache', 'files', 'logs', 'attachments', '.git']);
const hits = [];
let scanned = 0;

function walk(dir, depth) {
  if (depth > 6) return;
  let ents;
  try { ents = fs.readdirSync(dir, { withFileTypes: true }); } catch { return; }
  for (const e of ents) {
    const p = path.join(dir, e.name);
    if (e.isDirectory()) { if (!skipDirs.has(e.name)) walk(p, depth + 1); continue; }
    let st; try { st = fs.statSync(p); } catch { continue; }
    if (st.size > 30 * 1024 * 1024 || st.size < 2) continue;
    const ext = path.extname(e.name).toLowerCase();
    if (!['.json', '.yaml', '.yml', '.jsonl', '.txt', '.js', '.cjs', '.mjs', '.toml', '.db'].includes(ext)) continue;
    let buf; try { buf = fs.readFileSync(p); } catch { continue; }
    scanned++;
    let text;
    if (buf.length > 2 && buf[0] === 0xff && buf[1] === 0xfe) text = buf.toString('utf16le');
    else text = buf.toString('utf8');
    const idx = text.indexOf('llm-router');
    if (idx >= 0) {
      const has1048576 = text.includes('1048576');
      const has1049000 = text.includes('1049000');
      // find near-context of the llm-router occurrence containing maxTokens
      const around = text.slice(Math.max(0, idx - 400), idx + 2500);
      hits.push({ p, size: st.size, mtime: st.mtime.toISOString(), has1048576, has1049000, context: around.replace(/\s+/g, ' ').slice(0, 1200) });
    }
  }
}
walk(root, 0);
console.log('scanned=' + scanned + ' hits=' + hits.length);
for (const h of hits) {
  console.log('\n=== ' + h.p + '  size=' + h.size + ' mtime=' + h.mtime + ' has1048576=' + h.has1048576 + ' has1049000=' + h.has1049000);
  console.log('   ctx: ' + h.context);
}
