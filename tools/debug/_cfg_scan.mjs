import fs from 'node:fs';
const buf = fs.readFileSync('D:/Projects/llm-router/logs/_dsh_config.json');
let raw;
if (buf[0] === 0xff && buf[1] === 0xfe) raw = buf.subarray(2).toString('utf16le');
else if (buf[0] === 0xef) raw = buf.subarray(3).toString('utf8');
else raw = buf.toString('utf8');
console.log('HEAD:', JSON.stringify(raw.slice(0, 300)));
const lines = raw.split(/\r?\n/);
console.log('LINES:', lines.length);
let shown = 0;
lines.forEach((l, i) => {
  if (/maxTokens|max_tokens|maxOutputTokens|contextWindow|limits\b/i.test(l)) {
    if (shown++ < 200) console.log((i + 1) + ': ' + l.trimEnd());
  }
});
