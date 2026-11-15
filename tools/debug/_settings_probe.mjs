import fs from 'node:fs';

const files = [
  'C:/Users/17277/.dsh/settings.yaml',
  'C:/Users/17277/.dsh/profiles/tauri/cordis.patch.yml',
];
for (const f of files) {
  const raw = fs.readFileSync(f);
  const text = (raw[0] === 0xff && raw[1] === 0xfe) ? raw.toString('utf16le') : raw.toString('utf8');
  console.log('===== ' + f + '  bytes=' + raw.length);
  for (const needle of ['llm-router', 'llmrouter', 'deepseek']) {
    const idxs = [];
    let i = -1;
    while ((i = text.indexOf(needle, i + 1)) >= 0) idxs.push(i);
    console.log('  hits["' + needle + '"] = ' + idxs.length + (idxs.length ? ' first@' + idxs[0] : ''));
  }
  // print the block around each llm-router provider definition
  for (const needle of ['llm-router:', 'llmrouter:']) {
    let i = text.indexOf(needle);
    while (i >= 0) {
      const seg = text.slice(i, i + 400);
      const hasDeepseek = seg.includes('deepseek');
      console.log('  [' + needle + ' @' + i + '] deepseek-in-400ch=' + hasDeepseek + ' | ' + seg.replace(/\s+/g, ' ').slice(0, 200));
      i = text.indexOf(needle, i + 1);
    }
  }
}
