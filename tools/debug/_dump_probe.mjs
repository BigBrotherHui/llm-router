import fs from 'node:fs';

const p = 'D:/Projects/llm-router/logs/_dump_tauri.txt';
const text = fs.readFileSync(p, 'utf8');
// find every occurrence of 'llm-router' and print a window that includes model entries
const idxs = [];
let i = -1;
while ((i = text.indexOf('llm-router', i + 1)) >= 0) idxs.push(i);
console.log('llm-router occurrences: ' + idxs.length + ' at ' + idxs.join(','));
// print block around the providers definition (search for a 'deepseek' entry with maxTokens)
for (const needle of ['deepseek', 'DeepSeek-V4.1-Flash-1017', '1049000', '1048576', '393216', '131072']) {
  const c = text.split(needle).length - 1;
  console.log('count["' + needle + '"] = ' + c);
}
// locate the deepseek model entry: find 'id: deepseek' or '"id":"deepseek"' or similar
for (const re of [/id:\s*deepseek\b/g, /"id"\s*:\s*"deepseek"/g]) {
  let m;
  while ((m = re.exec(text))) {
    const start = Math.max(0, m.index - 200);
    console.log('\n--- match @' + m.index + ' ---\n' + text.slice(start, m.index + 700));
  }
}
