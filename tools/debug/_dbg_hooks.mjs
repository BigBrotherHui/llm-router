import fs from 'node:fs';
const lines = fs.readFileSync('D:/Projects/llm-router/logs/_dbg_game.jsonl', 'utf8').split('\n').filter(Boolean);
for (const s of lines) {
  let r; try { r = JSON.parse(s); } catch { continue; }
  if (/^hook\//.test(r.type)) console.log(r.type, 'seq=' + r.seq, JSON.stringify(r.data).slice(0, 300));
  if (r.type === 'session') console.log('session:', JSON.stringify(r).slice(0, 700));
}
