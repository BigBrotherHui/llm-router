// 把模型组声明的输出上限写进 llm-router 状态文件（上游硬上限保护）。
// 用法: node tools/debug/_set_cap.mjs <模型组名> <上限> [<模型组名> <上限> ...]
// 状态文件：环境变量 LLM_ROUTER_STATE 优先，否则用仓库根的 .llmrouter/config.state.json
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const STATE = process.env.LLM_ROUTER_STATE
  || path.resolve(HERE, '..', '..', '.llmrouter', 'config.state.json');
const args = process.argv.slice(2);
if (args.length === 0 || args.length % 2 !== 0) {
  console.error('usage: node _set_cap.mjs <group> <cap> [<group> <cap> ...]');
  process.exit(2);
}
const raw = fs.readFileSync(STATE, 'utf8');
const state = JSON.parse(raw);
const backup = STATE + '.capbak-' + Date.now();
fs.writeFileSync(backup, raw);

for (let i = 0; i < args.length; i += 2) {
  const group = args[i];
  const cap = Number(args[i + 1]);
  const route = state.models?.[group];
  if (!route) {
    console.error('!! 没有这个模型组: ' + group);
    process.exit(3);
  }
  const before = route.max_output_tokens;
  route.max_output_tokens = cap;
  const enabled = (route.entries || []).filter((e) => e.enabled && e.quota !== 0);
  const ups = [...new Set(enabled.map((e) => e.upstream))];
  console.log(`${group}: max_output_tokens ${before === undefined ? '(未设置)' : before} -> ${cap}  [enabled entries ${enabled.length}, upstreams ${ups.join(',')}]`);
}

fs.writeFileSync(STATE, JSON.stringify(state, null, 2));
console.log('备份: ' + backup);
console.log('已写入: ' + STATE);
