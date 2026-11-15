import fs from 'node:fs';
const raw = fs.readFileSync('D:/Projects/llm-router/.llmrouter/config.history.jsonl', 'utf8');
const lines = raw.trim().split('\n');
console.log('records:', lines.length);
const bySession = new Map();
for (const s of lines) {
  let r; try { r = JSON.parse(s); } catch { continue; }
  const k = (r.session_id || '-') + ' | src=' + (r.session_source || '-');
  if (!bySession.has(k)) bySession.set(k, { n: 0, models: new Set(), ok: 0, bad: 0, statuses: new Set(), first: r.time, last: r.time });
  const e = bySession.get(k);
  e.n++; e.models.add(r.model); e.statuses.add(r.status); if (r.ok) e.ok++; else e.bad++; e.last = r.time;
}
for (const [k, v] of bySession) console.log(`${k} :: n=${v.n} ok=${v.ok} bad=${v.bad} models=${[...v.models].join(',')} status=${[...v.statuses].join(',')} ${v.first}->${v.last}`);
// show the most recent 12 records compactly
console.log('\n--- last 12 ---');
for (const s of lines.slice(-12)) {
  let r; try { r = JSON.parse(s); } catch { continue; }
  console.log(`${r.time} sid=${(r.session_id || '-').slice(0, 34)} model=${r.model} ok=${r.ok} st=${r.status} up=${r.upstream} stream=${r.stream} err=${(r.error || '').slice(0, 90).replace(/\n/g, ' ')}`);
}
