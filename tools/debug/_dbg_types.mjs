import fs from 'node:fs';
const raw = fs.readFileSync('D:/Projects/llm-router/logs/_dbg_game.jsonl', 'utf8');
const lines = raw.split('\n').filter(Boolean);
const recs = lines.map((s) => { try { return JSON.parse(s); } catch { return { type: 'PARSE-ERR' }; } });
const types = {};
recs.forEach((r) => { types[r.type] = (types[r.type] || 0) + 1; });
console.log('TYPES:', JSON.stringify(types, null, 1));
const pats = ['max_completion', 'samplingParams', 'thinking_token_budget', 'max_tokens', 'temperature', 'prompt_cache_key', 'reasoning_effort', 'enable_thinking', 'thinkingBudget', 'thinking', 'contextWindow', 'body', 'params'];
for (const p of pats) console.log(p.padEnd(24), raw.split(p).length - 1);
// print keys of every distinct non-tool record shape
const seen = new Set();
for (const r of recs) {
  const k = r.type + '|' + Object.keys(r.data || {}).join(',');
  if (!seen.has(k)) { seen.add(k); console.log('SHAPE', k); }
}
