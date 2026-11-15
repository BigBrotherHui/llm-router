import fs from 'node:fs';

const files = [
  'C:/Users/17277/.dsh/profiles/tauri/cordis.patch.yml',
  'C:/Users/17277/.dsh/profiles/tauri/cordis.patch.yml.llmrouter.bak',
  'C:/Users/17277/.dsh/profiles/tauri/cordis.patch.yml.broken-20261115023044',
  'C:/Users/17277/.dsh/profiles/tauri/cordis.patch.yml.bak-20261101-125929',
  'C:/Users/17277/.dsh/settings.yaml.llmrouter.bak',
  'C:/Users/17277/.dsh/settings.yaml.imported.kept',
];
for (const f of files) {
  let raw;
  try { raw = fs.readFileSync(f); } catch (e) { console.log('=== ' + f + ' MISSING'); continue; }
  const text = (raw[0] === 0xff && raw[1] === 0xfe) ? raw.toString('utf16le') : raw.toString('utf8');
  const mtime = fs.statSync(f).mtime.toISOString();
  console.log('\n===== ' + f + '  bytes=' + raw.length + ' mtime=' + mtime);
  // print every 'id: deepseek' (or quoted) model entry block
  const re = /-?\s*id:\s*"?deepseek"?\s*\n([\s\S]{0,400}?)(?=\n\s*-?\s*id:|\n\n|$)/g;
  let m, n = 0;
  while ((m = re.exec(text)) && n < 6) {
    console.log('  --- entry ---\n' + m[0].replace(/\s+/g, ' ').slice(0, 300));
    n++;
  }
  if (!n) {
    // maybe llm-router defined with different quoting; show around 'llm-router'
    const i = text.indexOf('llm-router');
    console.log('  no id:deepseek match; llm-router@' + i);
    if (i >= 0) {
      const seg = text.slice(i, i + 20000);
      const j = seg.search(/deepseek["'?]?\s*\n/);
      console.log('  first deepseek-in-section@' + j);
      if (j >= 0) console.log('  ' + seg.slice(Math.max(0, j - 200), j + 400).replace(/\s+/g, ' '));
    }
  }
}
