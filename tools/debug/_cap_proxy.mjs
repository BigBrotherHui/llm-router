// Logging reverse proxy: captures whatever DSH/pi-ai puts on the wire, then
// forwards it verbatim to the real llm-router so behaviour is preserved.
import http from 'node:http';
import fs from 'node:fs';

// 可用环境变量覆盖：CAP_LISTEN / CAP_UPSTREAM / CAP_OUT（默认写到仓库 logs/）
//   node tools/debug/_cap_proxy.mjs
//   CAP_LISTEN=8322 CAP_UPSTREAM=http://127.0.0.1:8010 node tools/debug/_cap_proxy.mjs
const LISTEN = Number(process.env.CAP_LISTEN || 8321);
const UPSTREAM = process.env.CAP_UPSTREAM || 'http://127.0.0.1:8010';
const OUT_DIR = process.env.CAP_OUT || 'D:/Projects/llm-router/logs';
const LOG = `${OUT_DIR}/_capture_bodies.jsonl`;
const PRETTY_DIR = `${OUT_DIR}/_capture_bodies`;

let n = 0;
fs.mkdirSync(PRETTY_DIR, { recursive: true });

const server = http.createServer(async (req, res) => {
  const chunks = [];
  for await (const c of req) chunks.push(c);
  const raw = Buffer.concat(chunks).toString('utf8');
  n += 1;
  const stamp = new Date().toISOString().replace(/[:.]/g, '-');
  const file = `${PRETTY_DIR}/cap-${n}-${stamp}.json`;
  let parsed = null;
  try { parsed = JSON.parse(raw); } catch {}
  if (parsed) fs.writeFileSync(file, JSON.stringify(parsed, null, 2));

  const pick = (j) => j && ({
    model: j.model, stream: j.stream,
    max_tokens: j.max_tokens, max_completion_tokens: j.max_completion_tokens,
    temperature: j.temperature, top_p: j.top_p, reasoning_effort: j.reasoning_effort,
    thinking_token_budget: j.thinking_token_budget, thinking_budget: j.thinking_budget,
    enable_thinking: j.enable_thinking, store: j.store,
    stream_options: j.stream_options, tools: Array.isArray(j.tools) ? j.tools.length : j.tools,
    messages: Array.isArray(j.messages) ? j.messages.length : j.messages,
    payload_keys: j ? Object.keys(j) : null,
  });
  const summary = pick(parsed);
  console.log(`[cap ${n}] ${req.method} ${req.url} bytes=${raw.length} :: ${JSON.stringify(summary)}`);

  let upStatus = null;
  let upHead = '';
  try {
    const up = await fetch(UPSTREAM + req.url, {
      method: req.method,
      headers: { 'content-type': 'application/json', ...(req.headers.authorization ? { authorization: req.headers.authorization } : {}) },
      body: raw,
    });
    upStatus = up.status;
    res.writeHead(up.status, { 'content-type': up.headers.get('content-type') || 'application/json' });
    if (up.body) {
      const reader = up.body.getReader();
      for (;;) {
        const { done, value } = await reader.read();
        if (done) break;
        if (upHead.length < 400) upHead += Buffer.from(value).toString('utf8');
        res.write(Buffer.from(value));
      }
    }
    res.end();
  } catch (e) {
    upStatus = 'ERR ' + e.message;
    res.writeHead(502, { 'content-type': 'application/json' });
    res.end(JSON.stringify({ error: String(e) }));
  }

  fs.appendFileSync(LOG, JSON.stringify({
    n, at: stamp, url: req.url, bytes: raw.length,
    req_headers: { ...req.headers, authorization: req.headers.authorization ? '<redacted>' : undefined },
    params: summary, upstream_status: upStatus, upstream_head: upHead.slice(0, 300),
  }) + '\n');
  console.log(`[cap ${n}] upstream=${upStatus} head=${upHead.slice(0, 160).replace(/\s+/g, ' ')}`);
});

server.listen(LISTEN, '127.0.0.1', () => console.log('capture proxy listening on ' + LISTEN + ' -> ' + UPSTREAM));
