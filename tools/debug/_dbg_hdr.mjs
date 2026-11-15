import fs from 'node:fs';
const lines = fs.readFileSync('D:/Projects/llm-router/logs/_dbg_game.jsonl', 'utf8').split('\n').filter(Boolean);
for (const s of lines) {
  let r; try { r = JSON.parse(s); } catch { continue; }
  if (r.type === 'request/header') {
    const h = r.data.header;
    console.log('=== seq', r.seq, 'time', r.time, 'reason=', JSON.stringify(r.data.reason).slice(0, 200));
    console.log('HEADER KEYS:', Object.keys(h).join(', '));
    for (const k of Object.keys(h)) {
      if (k === 'tools' || k === 'messages' || k === 'system') { console.log(`  ${k}: <omitted len=${JSON.stringify(h[k]).length}>`); continue; }
      console.log(`  ${k} = ${JSON.stringify(h[k]).slice(0, 500)}`);
    }
  }
  if (r.type === 'session/title-llm-request') {
    console.log('=== title-llm-request:', JSON.stringify({ ...r.data, system: undefined, messages: undefined }).slice(0, 600));
  }
  if (r.type === 'model/selection') console.log('=== model/selection:', JSON.stringify(r.data));
  if (r.type === 'request/context') console.log('=== request/context:', JSON.stringify(r.data));
}
