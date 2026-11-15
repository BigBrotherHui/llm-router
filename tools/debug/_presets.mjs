import fs from 'node:fs';

const p = 'C:/Users/17277/.dsh/dsh-tauri-model/model-presets.json';
const raw = fs.readFileSync(p);
const text = (raw[0] === 0xff && raw[1] === 0xfe) ? raw.toString('utf16le') : raw.toString('utf8');
console.log('bytes=' + raw.length + ' head=' + text.slice(0, 300).replace(/\s+/g, ' '));
const j = JSON.parse(text);
console.log('top keys: ' + Object.keys(j).join(','));
const presets = j.presets || j.models || j.data || j;
console.log('presets type=' + (Array.isArray(presets) ? 'array len=' + presets.length : 'object keys=' + Object.keys(presets).length));
const entries = Array.isArray(presets) ? presets.map((v, i) => [String(i), v]) : Object.entries(presets);
let n = 0;
for (const [k, v] of entries) {
  const s = JSON.stringify(v);
  if (/deepseek/i.test(k) || /1049000/.test(s)) {
    console.log('  ' + k + ' => ' + s.slice(0, 260));
    n++;
    if (n > 40) break;
  }
}
console.log('matched=' + n + ' total=' + entries.length);
