// Does ModelScope's "Range of max_completion_tokens should be [1, 393216]" error
// depend on the PROMPT size rather than on the requested value itself?
const KEY = process.env.MS_KEY || '';
if (!KEY) { console.error('MS_KEY 未设置（例如 MS_KEY=ms-xxx MAXC=131072 node tools/debug/_prob4.mjs）'); process.exit(2); }
const URL = 'https://api-inference.modelscope.cn/v1/chat/completions';
const MODEL = 'deepseek-ai/DeepSeek-V4.1-Flash-1017';

function blob(targetTokens) {
  const chars = Math.round(targetTokens * 3.5);
  const unit = 'The quick brown fox jumps over the lazy dog 0123456789. ';
  let s = '';
  while (s.length < chars) s += unit;
  return s.slice(0, chars);
}

const sizes = process.argv.slice(2).map(Number);
const maxTokens = Number(process.env.MAXC || 131072);

for (const t of sizes) {
  const body = {
    model: MODEL,
    messages: [
      { role: 'system', content: 'You are a helpful assistant. Answer with one word.' },
      { role: 'user', content: blob(t) + '\n\nIgnore the text above. Reply with exactly: ok' },
    ],
    max_completion_tokens: maxTokens,
    stream: false,
  };
  const bytes = Buffer.byteLength(JSON.stringify(body));
  const t0 = Date.now();
  try {
    const r = await fetch(URL, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json', Authorization: 'Bearer ' + KEY },
      body: JSON.stringify(body),
    });
    const txt = await r.text();
    let note = txt.slice(0, 300).replace(/\s+/g, ' ');
    try {
      const j = JSON.parse(txt);
      if (j.error) note = JSON.stringify(j.error).slice(0, 300);
      else if (j.choices) note = 'content="' + (j.choices[0]?.message?.content || '').slice(0, 40) + '" usage=' + JSON.stringify(j.usage || {});
    } catch {}
    console.log(`prompt~${t}tok bytes=${bytes} maxc=${maxTokens} -> ${r.status} ${((Date.now() - t0) / 1000).toFixed(1)}s :: ${note}`);
  } catch (e) {
    console.log(`prompt~${t}tok bytes=${bytes} -> ERR ${e.message}`);
  }
}
