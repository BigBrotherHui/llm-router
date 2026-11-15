// Which malformed/out-of-range shapes produce ModelScope's exact
// "Range of max_completion_tokens should be [1, 393216]" error?
const KEY = process.env.MS_KEY || '';
if (!KEY) { console.error('MS_KEY 未设置（例如 MS_KEY=ms-xxx node tools/debug/_prob5.mjs）'); process.exit(2); }
const URL = 'https://api-inference.modelscope.cn/v1/chat/completions';
const MODEL = 'deepseek-ai/DeepSeek-V4.1-Flash-1017';

const cases = [
  ['mct=393217 (just over)', { max_completion_tokens: 393217 }],
  ['mct=393216 (boundary)', { max_completion_tokens: 393216 }],
  ['mct=2097152 (2M)', { max_completion_tokens: 2097152 }],
  ['mct=0', { max_completion_tokens: 0 }],
  ['mct=-1', { max_completion_tokens: -1 }],
  ['mct=null', { max_completion_tokens: null }],
  ['mct="131072" (string)', { max_completion_tokens: '131072' }],
  ['mct=131072.5 (float)', { max_completion_tokens: 131072.5 }],
  ['mct=131072 + max_tokens=131072', { max_completion_tokens: 131072, max_tokens: 131072 }],
  ['mct=131072 + max_tokens=400000', { max_completion_tokens: 131072, max_tokens: 400000 }],
  ['max_tokens=400000 only', { max_tokens: 400000 }],
  ['mct=131072 + reasoning_effort=max', { max_completion_tokens: 131072, reasoning_effort: 'max' }],
];

for (const [label, extra] of cases) {
  const body = { model: MODEL, messages: [{ role: 'user', content: 'Reply with exactly: ok' }], stream: false, ...extra };
  try {
    const r = await fetch(URL, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json', Authorization: 'Bearer ' + KEY },
      body: JSON.stringify(body),
    });
    const txt = await r.text();
    let note = txt.slice(0, 240).replace(/\s+/g, ' ');
    try {
      const j = JSON.parse(txt);
      if (j.error) note = 'ERRCODE=' + j.error.code + ' MSG=' + j.error.message;
      else if (j.choices) note = 'OK content="' + (j.choices[0]?.message?.content || '').slice(0, 30) + '"';
    } catch {}
    console.log(`${String(r.status).padEnd(4)} ${label.padEnd(34)} :: ${note}`);
  } catch (e) {
    console.log(`ERR  ${label.padEnd(34)} :: ${e.message}`);
  }
  await new Promise((s) => setTimeout(s, 900));
}
