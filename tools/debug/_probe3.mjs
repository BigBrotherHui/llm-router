const ROUTER = 'http://127.0.0.1:8010/v1/chat/completions';

const tools = [
  {
    type: 'function',
    function: {
      name: 'run_code',
      description: 'Run a snippet of code in the sandbox.',
      parameters: {
        type: 'object',
        properties: { code: { type: 'string' }, language: { type: 'string' } },
        required: ['code'],
      },
    },
  },
];

function body(over) {
  return {
    model: 'deepseek',
    messages: [
      { role: 'system', content: 'You are a helpful coding agent.' },
      { role: 'user', content: 'say ok' },
    ],
    stream: true,
    stream_options: { include_usage: true },
    max_completion_tokens: 131072,
    ...over,
  };
}

const variants = {
  minimal: {},
  storeFalse: { store: false },
  reasoningEffort: { reasoning_effort: 'max' },
  tools: { tools },
  all: { store: false, reasoning_effort: 'max', tools },
  altMaxTokens: { store: false, reasoning_effort: 'max', tools, max_tokens: 131072 },
};

const pick = process.argv[2] || 'all';
const payload = body(variants[pick] || {});
const t0 = Date.now();
const r = await fetch(ROUTER, {
  method: 'POST',
  headers: { 'Content-Type': 'application/json', Authorization: 'Bearer test', 'x-session-id': 'probe-' + pick },
  body: JSON.stringify(payload),
});
const txt = await r.text();
console.log(`[${pick}] status=${r.status} ${((Date.now() - t0) / 1000).toFixed(2)}s :: ${txt.slice(0, 400).replace(/\s+/g, ' ')}`);
