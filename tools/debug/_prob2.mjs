// 直连上游探针；key 从环境变量取，不要把真实 key 写进仓库：
//   MS_KEY=ms-xxx node tools/debug/_prob2.mjs
const KEY = process.env.MS_KEY || '';
if (!KEY) { console.error('MS_KEY 未设置（例如 MS_KEY=ms-xxx node tools/debug/_prob2.mjs）'); process.exit(2); }
const URL = 'https://api-inference.modelscope.cn/v1/chat/completions';
const MODEL = 'deepseek-ai/DeepSeek-V4.1-Flash-1017';

async function probe(name, extra) {
  const body = { model: MODEL, messages: [{ role: 'user', content: 'say ok' }], stream: false, ...extra };
  const t0 = Date.now();
  try {
    const r = await fetch(URL, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json', Authorization: 'Bearer ' + KEY },
      body: JSON.stringify(body),
    });
    const txt = await r.text();
    console.log(`[${name}] status=${r.status} ${((Date.now() - t0) / 1000).toFixed(2)}s :: ${txt.slice(0, 260).replace(/\s+/g, ' ')}`);
  } catch (e) {
    console.log(`[${name}] FETCH-ERR ${e.message}`);
  }
}

const which = process.argv[2] || 'all';
if (which === 'a') await probe('max_completion_tokens=131072 + reasoning_effort=max', { max_completion_tokens: 131072, reasoning_effort: 'max' });
if (which === 'b') await probe('reasoning_effort=max only', { reasoning_effort: 'max' });
if (which === 'c') await probe('131072 + thinking_token_budget=262144', { max_completion_tokens: 131072, thinking_token_budget: 262144 });
if (which === 'd') await probe('131072 baseline control', { max_completion_tokens: 131072 });
