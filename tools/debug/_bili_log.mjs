import fs from 'node:fs';

const p = 'C:/Users/17277/.local/state/billion-context/bili.log';
const st = fs.statSync(p);
console.log('size=' + st.size + ' mtime=' + st.mtime.toISOString());
const text = fs.readFileSync(p, 'utf8');
const lines = text.split(/\r?\n/);
console.log('lines=' + lines.length);
const pats = [/output budget/i, /ceiling/i, /max_completion/i, /1048576/, /1018356/, /clamp/i, /restore/i, /deepseek/];
const counts = pats.map(p => [p.source, lines.filter(l => p.test(l)).length]);
console.log('counts: ' + JSON.stringify(counts));
// print the last N lines that mention output budget / ceiling / clamp / 1048576
const interesting = lines.filter(l => /output budget|ceiling|clamp|1048576|1018356|max_completion/i.test(l));
console.log('interesting lines=' + interesting.length);
for (const l of interesting.slice(-60)) console.log('  ' + l.slice(0, 400));
console.log('\n=== last 25 raw lines ===');
for (const l of lines.slice(-25)) console.log('  ' + l.slice(0, 300));
