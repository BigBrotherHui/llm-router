import fs from 'node:fs';

const dir = 'C:/Users/17277/.local/share/billion-context/sessions/openai';
const files = [
  '127.0.0.1_9da8b84d10110586ce400dc4.json', // Game session (mtime 14:47:10)
  '127.0.0.1_291304849237dce0ddd721cc.json', // probe #1 (15:05:51)
  '127.0.0.1_72c65cbc450cf0cc1947e5f7.json', // probe #2 (15:06:59)
];

for (const f of files) {
  const p = dir + '/' + f;
  const raw = fs.readFileSync(p, 'utf8');
  console.log('==================== ' + f + '  bytes=' + raw.length);
  let j = null;
  try { j = JSON.parse(raw); } catch (e) { console.log('NOT single JSON: ' + e.message + '  (lines=' + raw.split('\n').length + ')'); }
  if (j) {
    console.log('top keys: ' + Object.keys(j).join(','));
    const md = j.metadata || j.meta || null;
    if (md) console.log('metadata keys: ' + Object.keys(md).join(','));
    const dump = (o, prefix) => {
      for (const [k, v] of Object.entries(o || {})) {
        if (v !== null && typeof v === 'object' && !Array.isArray(v)) dump(v, prefix + k + '.');
        else if (typeof v !== 'function') {
          const s = Array.isArray(v) ? '[array ' + v.length + ']' : String(v);
          if (/window|output|budget|token|ceiling|model|high|water|clamp|context|compress|restore|plugin/i.test(k) || /1048|39321|10183|1049/.test(s))
            console.log('  ' + prefix + k + ' = ' + s.slice(0, 200));
        }
      }
    };
    dump(md && typeof md === 'object' ? md : j, '');
    if (Array.isArray(j.messages)) console.log('messages: ' + j.messages.length);
    if (Array.isArray(j.entries)) console.log('entries: ' + j.entries.length);
  }
}
