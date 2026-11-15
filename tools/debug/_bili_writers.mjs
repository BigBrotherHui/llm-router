// Targeted hunt: who WRITES max_completion_tokens in billion-context.
import fs from 'fs';
import path from 'path';
import { execSync } from 'child_process';

const ROOT = 'C:\\Users\\17277\\.dsh\\profiles\\tauri\\node_modules\\billion-context';
const out = [];
function walk(dir, acc = []) {
  for (const e of fs.readdirSync(dir, { withFileTypes: true })) {
    const p = path.join(dir, e.name);
    if (e.isDirectory()) walk(p, acc);
    else if (/\.(js|mjs|cjs)$/.test(e.name)) acc.push(p);
  }
  return acc;
}
const files = walk(ROOT).filter(f => !f.includes('opencode-native') && !f.includes('omp-native'));
for (const f of files) {
  const txt = fs.readFileSync(f, 'utf8');
  const rel = path.relative(ROOT, f);
  // assignment-style occurrences
  const re = /max_completion_tokens/gi;
  let m;
  while ((m = re.exec(txt)) !== null) {
    const i = m.index;
    const after = txt.slice(i + 20, i + 60);
    const before = txt.slice(Math.max(0, i - 120), i + 20);
    const isWrite = /^\s*["'\]]*\s*[=:]\s*/.test(after) || /\[\s*["']max_completion_tokens["']\s*\]\s*=/.test(before + 'x');
    if (isWrite) out.push(`[WRITE?] ${rel}@${i}\n  before: ...${before.replace(/\s+/g, ' ')}\n  after : ${after.replace(/\s+/g, ' ')}`);
  }
  for (const key of ['OUTPUT_LEDGER', 'outputBudgetField', 'raiseOutput', 'declaredWindowForDirect', 'ensureBudget', 'outputBudget']) {
    let i = -1, n = 0;
    while ((i = txt.indexOf(key, i + 1)) !== -1 && n < 4) {
      n++;
      out.push(`[KEY ${key}] ${rel}@${i}\n  ...${txt.slice(Math.max(0, i - 200), i + 200).replace(/\s+/g, ' ')}...`);
    }
  }
}
fs.writeFileSync('D:\\Projects\\llm-router\\logs\\_bili_writers.txt', out.join('\n\n'), 'utf8');
console.log('total findings:', out.length);
console.log(out.slice(0, 40).join('\n\n'));
