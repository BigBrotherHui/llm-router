"use strict";
const $ = s => document.querySelector(s);
// 手机链路挂在 opencode-web 的 /llm/ 反代下时，API 走同一前缀（同源才能复用推送）
const apiBase = location.pathname.startsWith("/llm") ? "/llm" : "";
// 本机 / 桌面控制台才显示「本机 Agent」页：中继页没有这些本地接口。
const AGENT_LOCAL = ["127.0.0.1", "localhost", "[::1]", "::1"].includes(location.hostname)
  || new URLSearchParams(location.search).has("native");
let state = null;
let pageVersion = null;  // 服务端版本号，变了就自动重载（避免旧 JS 配新后端）
let editing = null;      // { name, strategy, picks:[{id,name,hasModel,model,enabled}] }
let lastTest = {};
let lastTestByModel = {};   // 每个模型各测一次的结果，key = "上游id|模型名"
let serverUrl = location.origin;

function pad2(n) { return String(n).padStart(2, "0"); }
function dayKeyOf(atSeconds) {
  const d = atSeconds ? new Date(atSeconds * 1000) : new Date();
  return `${d.getFullYear()}-${pad2(d.getMonth() + 1)}-${pad2(d.getDate())}`;
}
// 哪些上游卡片展开了模型勾选面板；以及各面板里输入的筛选词
const expanded = {};
const pickFilter = {};
let upSortDir = 1;   // 上游来源卡片排序方向：1 = 原始顺序，-1 = 倒过来

// ---- 日志：界面上弹过的提示都留一份，「日志」tab 里能慢慢翻 ----
// 存 localStorage（上限 300 条），页面刷新/版本自动重载后也还在。
const LOG_KEY = "llm-router-log";
const LOG_MAX = 300;
let logEntries = [];
try {
  const saved = JSON.parse(localStorage.getItem(LOG_KEY) || "[]");
  logEntries = Array.isArray(saved) ? saved : [];
} catch (e) { logEntries = []; }
let logUnread = 0;   // 没看「日志」tab 时新攒的条数，tab 上显示小数字

function logAdd(kind, msg) {
  logEntries.push({ at: Date.now(), kind: kind || "", msg: String(msg).slice(0, 4000) });
  if (logEntries.length > LOG_MAX) logEntries = logEntries.slice(-LOG_MAX);
  try { localStorage.setItem(LOG_KEY, JSON.stringify(logEntries)); } catch (e) {}
  if (curTab === "log") {
    renderLog();
  } else {
    logUnread += 1;
    setTabNum("log", logUnread, false, "未看的新日志条数");
  }
}

function renderLog() {
  const box = $("#logList");
  if (!box) return;
  const n = logEntries.length;
  const cnt = $("#logCount");
  if (cnt) cnt.textContent = n ? `共 ${n} 条` : "还没有日志";
  box.innerHTML = "";
  [...logEntries].reverse().forEach(e => {
    const d = new Date(e.at);
    const color = e.kind === "bad" ? "var(--bad)"
                : e.kind === "ok" ? "var(--ok)"
                : e.kind === "warn" ? "var(--warn)" : "var(--dim)";
    const mark = e.kind === "bad" ? "✗" : e.kind === "ok" ? "✓"
               : e.kind === "warn" ? "!" : "·";
    const row = document.createElement("div");
    row.style.cssText = "display:flex;gap:8px;align-items:flex-start;" +
      "border-bottom:1px solid var(--line);padding:5px 0";
    row.innerHTML = `<span class="muted" style="flex:none">${
      d.getFullYear()}-${pad2(d.getMonth() + 1)}-${pad2(d.getDate())} ${pad2(d.getHours())}:${pad2(d.getMinutes())}:${pad2(d.getSeconds())}</span>` +
      `<span style="flex:none;color:${color}">${mark}</span>` +
      `<span style="white-space:pre-wrap;word-break:break-word">${esc(e.msg)}</span>`;
    box.appendChild(row);
  });
}

function toast(msg, kind = "") {
  logAdd(kind, msg);
  const el = document.createElement("div");
  el.className = kind;
  el.textContent = msg;
  $("#toast").appendChild(el);
  const life = kind === "bad" ? 6500 : 3600;
  setTimeout(() => { el.style.opacity = "0"; setTimeout(() => el.remove(), 250); }, life);
}
function esc(s) {
  return String(s == null ? "" : s).replace(/[&<>"']/g, c =>
    ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
}
// ---------------- 多线路（镜像）实时排名 ----------------
// 只在配了 2 条以上线路时出现；显示每条线路的实时延迟、谁最快、谁没接口。
// 注意：必须放在顶层（和 esc 一起），不能塞进文件末尾的 boot()。
function renderMirrors(u) {
  const ms = u.mirrors || [];
  if (ms.length < 2) return "";
  const sorted = [...ms].sort((a, b) => a.score - b.score);
  const alive = ms.filter(m => !m.dead && m.available).length;
  const best = sorted.find(m => !m.dead) || null;

  const rows = sorted.slice(0, 6).map(m => {
    const lat = m.ewma_ms == null ? "—" : m.ewma_ms + "ms";
    let tag = "";
    if (m.dead) tag = `<span style="color:var(--bad)">无接口</span>`;
    else if (!m.available) tag = `<span style="color:var(--warn)">冷却 ${Math.ceil(m.cooldown_remaining_seconds || 0)}s</span>`;
    else if (best && m.url === best.url) tag = `<span style="color:var(--ok)">★ 最快</span>`;
    else if (m.url === u.last_mirror) tag = `<span style="color:var(--accent)">刚用过</span>`;
    const dim = m.dead ? "opacity:.45;" : "";
    return `<div style="display:flex;gap:8px;align-items:baseline;${dim}">` +
      `<span style="flex:1;min-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;color:var(--dim)">${esc(m.url)}</span>` +
      `<span style="min-width:58px;text-align:right">${lat}</span>` +
      `<span style="min-width:62px;text-align:right;font-size:11px">${tag}</span></div>`;
  }).join("");

  const more = ms.length > 6 ? `<div class="hint">…还有 ${ms.length - 6} 条</div>` : "";
  const strat = (u.mirror_strategy && u.mirror_strategy !== "fastest")
    ? "（策略 " + esc(u.mirror_strategy) + "）" : "";
  return `<div style="margin-top:8px;padding:8px 10px;background:rgba(255,255,255,.03);` +
    `border:1px solid var(--line);border-radius:8px;font-family:ui-monospace,Consolas,monospace;` +
    `font-size:11.5px;line-height:1.75">` +
    `<div class="hint" style="margin-bottom:5px;font-family:inherit">多线路 · 在线 ${alive}/${ms.length} · 自动走最快${strat}</div>` +
    `${rows}${more}</div>`;
}
function fmtLat(v) {
  if (v == null) return "—";
  if (v < 1) return Math.round(v * 1000) + " ms";
  return v.toFixed(2) + " s";
}
function latColor(v) {
  if (v == null) return "var(--dim)";
  if (v < 1.2) return "var(--ok)";
  if (v < 3) return "var(--warn)";
  return "var(--bad)";
}
function whenText(ts) {
  if (!ts) return "从未";
  const d = Math.floor(Date.now() / 1000 - ts);
  if (d < 60) return "刚刚";
  if (d < 3600) return Math.floor(d / 60) + " 分钟前";
  if (d < 86400) return Math.floor(d / 3600) + " 小时前";
  return Math.floor(d / 86400) + " 天前";
}

async function api(path, body, method) {
  const opts = { method: method || (body === undefined ? "GET" : "POST"), headers: {} };
  if (body !== undefined) { opts.headers["Content-Type"] = "application/json";
    opts.body = JSON.stringify(body); }
  const res = await fetch(apiBase + path, opts);
  let data = null;
  try { data = await res.json(); } catch (e) { data = null; }
  if (!res.ok) {
    const msg = (data && data.error && (data.error.message || data.error)) || ("HTTP " + res.status);
    throw new Error(typeof msg === "string" ? msg : JSON.stringify(msg));
  }
  return data;
}

/* ================= 渲染：上游 ================= */
// 记录上一次渲染出来的 HTML：内容没变就完全不碰 DOM。
// 否则每 2.5 秒把节点全部重建，用户正好点到一半会被换掉，点击就丢了。
// 同理，勾选模型也会重渲染，所以要把滚动位置和光标位置一起保住。
const lastHtml = { upstreams: null, models: null };
// 各上游勾选面板的滚动位置
const pickScroll = {};

function applyHtml(key, container, html) {
  if (lastHtml[key] === html) return false;
  lastHtml[key] = html;
  container.innerHTML = html;
  return true;
}

// 重渲染后把勾选面板的滚动位置还原回去（模型多的上游会很长）
function restorePickScroll(root) {
  root.querySelectorAll(".up").forEach(card => {
    const list = card.querySelector(".picklist");
    if (!list) return;
    const top = pickScroll[card.dataset.id];
    if (top) list.scrollTop = top;
    // 滚动时就记下来，供下次重渲染还原
    list.addEventListener("scroll", () => {
      pickScroll[card.dataset.id] = list.scrollTop;
    }, { passive: true });
  });
}

// 每个模型的测速结果：一行一个模型，标出 可用/不可用/限流 + 延迟。
// 数据来源：本次会话刚测的(lastTestByModel)，或持久化的 u.last_checks。
function modelCheckEntry(u, m) {
  return lastTestByModel[`${u.id}|${m}`]
    || (u.last_checks || {})[m]
    || null;
}
function checkBadge(c) {
  if (!c) return "";
  let label, color;
  if (c.rate_limited) { label = "本次测试：限流"; color = "var(--warn)"; }
  else if (c.ok) { label = "本次测试：成功（短请求)"; color = "var(--ok)"; }
  else { label = "本次测试：失败"; color = "var(--bad)"; }
  const lat = (c.ok && c.latency_seconds != null) ? " · 耗时 " + fmtLat(c.latency_seconds) : "";
  const tip = [explainHttpError(c.error, c.status), "仅测试短请求，不代表长上下文、工具调用或流式请求一定成功"].filter(Boolean).join("；");
  const actual = c.last_actual;
  const history = c.warning ? `<span class="badge mono warn" title="${esc(c.warning)}">历史调用：最近一次失败${actual && actual.status ? " HTTP " + esc(actual.status) : ""}${actual && actual.at ? "(" + esc(whenText(actual.at)) + ")" : ""}</span>` : "";
  return `<span class="badge mono" style="color:${color};border-color:currentColor" title="${esc(tip)}">${label}${lat}</span>${history}`;
}
// 该上游勾选的每个模型各一行测试结果(没测过的也列出来，提示去测)
function renderCheckResults(u) {
  const sel = u.selected_models || [];
  if (!sel.length) return "";
  const rows = sel.map(m => {
    const c = modelCheckEntry(u, m);
    return `<span class="mchip" style="display:inline-flex;align-items:center;flex-wrap:wrap;gap:5px">${esc(m)} ${c ? checkBadge(c) : '<span class="badge" style="color:var(--dim)">未测</span>'}</span>`;
  }).join("");
  return `<div class="models-wrap" style="margin-top:8px">${rows}</div>`;
}
// 余额标记：查过的显示金额，没查过/查不到显示「—」。
// 各厂商余额接口不统一，只有 New API / One API 那套能查（见后端 fetch_balance)。
function moneyText(v) {
  if (v == null || isNaN(v)) return "—";
  return "$" + Number(v).toFixed(2);
}
function balanceBadge(u) {
  const b = u.balance;
  if (!b) {
    return `<span class="badge mono" title="还没查过余额，点「查余额」">余额 —</span>`;
  }
  if (!b.ok) {
    return `<span class="badge" style="color:var(--dim)" title="${esc(b.error || "查不到")}">余额 —</span>`;
  }
  const rem = moneyText(b.remaining);
  const tip = `总额度 ${moneyText(b.total)} · 已用 ${moneyText(b.used)}` +
              (b.at ? ` · 更新于 ${esc(whenText(b.at))}` : "");
  const low = (b.remaining != null && b.remaining <= 0);
  return `<span class="badge ${low ? "warn" : "ok"}" title="${esc(tip)}">余额 ${rem}</span>`;
}

// 卡片上的 Key 徽标：多 Key 显示个数+策略（带禁用数），单 Key 显示名字（没名字显示打码 Key）
function keyBadge(u) {
  const names = u.key_names || [];
  const hints = u.key_hints || [];
  const dis = u.key_disabled || [];
  const nDis = dis.filter(Boolean).length;
  const detail = hints.map((h, i) =>
    (names[i] ? names[i] + " " : "") + h + (dis[i] ? "（禁用）" : "")).join("\uff0c");
  if (u.key_count > 1) {
    const how = u.key_strategy === "round_robin" ? "\u6bcf\u6b21\u8bf7\u6c42\u8f6e\u6d41\u7528" : "\u4e00\u4e2a\u51fa\u9519\u518d\u5207\u4e0b\u4e00\u4e2a";
    const off = nDis ? `\uff08${nDis} \u7981\u7528\uff09` : "";
    return `<span class="badge" title="${esc(how + (detail ? "\uff1b" + detail : ""))}">${u.key_count} \u4e2a Key${off}${u.key_strategy === "round_robin" ? " \u21bb" : " \u2192"}</span>`;
  }
  if (!u.has_key) return `<span class="badge">\u65e0 Key</span>`;
  const off = dis[0] ? "\uff08\u5df2\u7981\u7528\uff09" : "";
  const label = (names[0] || u.key_hint) + off;
  return `<span class="badge" title="${esc(names[0] ? u.key_hint : "")}">${esc(label)}</span>`;
}

// 卡片上的出口徽标：这个上游实际从哪儿出去（直连 / 规则 / 强制代理）。
// 悬停能看到具体端口和来源（跟随全局 / 单独指定）。
// 出口控制整体关着时显示「未控制」—— 那是实话：它确实不显式用代理，
// 但 TUN 下走哪儿由 Clash 规则决定，标成「直连」会误导人。
function netExitBadge(u) {
  const mode = u.net_mode_effective || "rule";
  const src = u.proxy_source || "none";
  const on = !!(state && state.proxy && state.proxy.enabled);
  const label = (src === "none" && !on)
    ? "未控制"
    : ({ direct: "直连", rule: "规则", proxy: "强制代理" }[mode] || mode);
  let tip = `出口：${label}`;
  if (src === "none" && !on) {
    tip += "（出口控制关着，去处由系统 / Clash 决定）";
  } else {
    tip += u.proxy_effective ? ` → ${u.proxy_effective}` : "（不显式用代理）";
    if (mode === "direct" && !u.proxy_effective) {
      tip += " · 没有配「直连端口」时，TUN 模式下仍会被 Clash 规则接管";
    }
    if (src === "upstream") tip += " · 这个上游单独指定的";
    else if (src === "global") tip += " · 用「设置」页里的全局端口";
  }
  if (u.proxy_error) tip += ` · 配置有误：${u.proxy_error}`;
  const cls = u.proxy_error ? "bad"
    : (label === "未控制" ? "" : (mode === "direct" ? "ok" : (mode === "proxy" ? "warn" : "")));
  return `<span class="badge ${cls}" title="${esc(tip)}" style="cursor:help">` +
         `${esc(label)}${src === "none" ? "" : " ⇄"}</span>`;
}

function hueOf(s) {
  let h = 0;
  for (const ch of String(s == null ? "" : s)) h = (h * 31 + ch.codePointAt(0)) % 360;
  return h;
}
function initialOf(s) {
  const t = String(s == null ? "" : s).trim();
  return (Array.from(t)[0] || "?").toUpperCase();
}

function renderUpstreams() {
  const ups = state.upstreams || [];
  // 用户正在分组输入框里操作（或下拉开着）时不要重建 DOM，
  // 否则 2.5 秒一次的自动刷新会把输入框连着他正在打的内容一起换掉
  const active = document.activeElement;
  if (groupMenuFor || (active && active.matches && active.matches("input[data-group-for]"))) {
    return;
  }
  if (!ups.length) {
    applyHtml("upstreams", $("#upstreams"), `<div class="empty">
      还没有上游来源。<br>在上面填「名字 / 地址 / Key」，点「添加」就行。</div>`);
    return;
  }
  const q = (($("#upSearch") && $("#upSearch").value) || "").trim().toLowerCase();
  // 通用搜索：上游名字 / 地址 / 已选模型的真实名 / 分组名
  const hit = u => {
    if (!q) return true;
    if ((u.name || "").toLowerCase().includes(q)) return true;
    if ((u.base_url || "").toLowerCase().includes(q)) return true;
    const sel = u.selected_models || [];
    const groups = u.model_groups || {};
    return sel.some(m => m.toLowerCase().includes(q)
                       || (groups[m] || "").toLowerCase().includes(q));
  };
  // 模型搜索（独立输入框）：只筛「支持这个模型」的上游，含已拉取但未勾选的。
  // 和通用搜索分开，避免搜模型名时把名字/地址碰巧相同的上游混进来。
  const mq = (($("#upModelFind") && $("#upModelFind").value) || "").trim().toLowerCase();
  const supports = u => {
    if (!mq) return true;
    const sel = u.selected_models || [];
    const avail = u.available_models || [];
    return sel.some(m => m.toLowerCase().includes(mq))
        || avail.some(m => m.toLowerCase().includes(mq));
  };
  // 模型搜索的分类：selected=已勾选，available=支持但未勾选
  const matchType = u => {
    if (!mq) return "none";
    const sel = u.selected_models || [];
    if (sel.some(m => m.toLowerCase().includes(mq))) return "selected";
    const avail = u.available_models || [];
    if (avail.some(m => m.toLowerCase().includes(mq))) return "available";
    return "none";
  };
  const shown = ups.filter(hit).filter(supports);
  // 启用状态筛选：hide=不含停用（默认）/ all=包含停用 / only=只看停用
  const ef = ($("#upEnabledFilter") && $("#upEnabledFilter").value) || "hide";
  const byEnabled = u => ef === "all" ? true : (ef === "only" ? !u.enabled : u.enabled);
  const filtered = shown.filter(byEnabled);
  // 模型搜索时排序：支持但未选排最前（发现价值大），已选其次
  if (mq) {
    const order = { available: 0, selected: 1, none: 2 };
    filtered.sort((a, b) => (order[matchType(a)] ?? 2) - (order[matchType(b)] ?? 2));
  }
  if (upSortDir < 0) filtered.reverse();
  if (!filtered.length) {
    const why = mq ? `没有上游支持模型「${esc(mq)}」。`
                  : (q ? `没有匹配「${esc(q)}」的上游。`
                       : (ef === "only" ? "没有已停用的上游。" : "没有可显示的上游。"));
    applyHtml("upstreams", $("#upstreams"), `<div class="empty">${why}</div>`);
    return;
  }
  shown.length = 0; shown.push(...filtered);
  // 模型搜索激活时：图例 + 统计
  const uc = $("#upCount");
  if (uc && mq) {
    const nSel = shown.filter(u => matchType(u) === "selected").length;
    const nAvail = shown.filter(u => matchType(u) === "available").length;
    uc.innerHTML = `<span style="color:#16a34a">\u2588 \u5df2\u9009 ${nSel}</span>` +
      ` &nbsp;<span style="color:#b45309">\u2588 \u652f\u6301\u4f46\u672a\u9009 ${nAvail}</span>` +
      ` &nbsp;\u2014 \u5171 ${shown.length} / ${ups.length}`;
  } else if (uc) {
    const nOn = ups.filter(u => u.enabled).length;
    uc.textContent = ups.length ? `${ups.length} \u4e2a\u4e0a\u6e38\uff0c\u5176\u4e2d ${nOn} \u4e2a\u5df2\u542f\u7528` : "";
  }
  const html = shown.map(u => {
    const t = lastTest[u.id];
    const cooling = u.cooldown_remaining_seconds > 0;
    const mt = mq ? matchType(u) : "none";
    const cls = ["up", u.enabled ? "" : "off", cooling ? "cool" : "",
                 expanded[u.id] ? "open" : "",
                 mt === "available" ? "match-avail" : "",
                 mt === "selected" ? "match-sel" : ""].join(" ");
    const nAvail = u.available_count != null ? u.available_count : (u.available_models || []).length;
    const nSel = u.selected_count != null ? u.selected_count : (u.selected_models || []).length;
    const lat = u.avg_latency_seconds;
    const maxLat = Math.max(...ups.map(x => x.avg_latency_seconds || 0), 0.001);
    const open = expanded[u.id];
    const hue = hueOf(u.name);
    const statusBadge = u.enabled
      ? (cooling ? `<span class="badge warn">冷却中 ${u.cooldown_remaining_seconds}s</span>`
                 : `<span class="badge ok">● 启用</span>`)
      : `<span class="badge bad">已停用</span>`;

    return `
    <div class="${cls}" data-id="${esc(u.id)}" style="--h:${hue}">
      <div class="up-head">
        <span class="up-avatar" aria-hidden="true">${esc(initialOf(u.name))}</span>
        <div class="up-identity">
          <div class="up-name-row"><span class="up-name" title="${esc(u.name)}">${esc(u.name)}</span></div>
          <div class="url" title="${esc(u.base_url)}">${esc(u.base_url)}</div>
        </div>
      </div>
      <div class="up-badges">
        ${statusBadge}
        ${mt === "available" ? `<span class="badge" style="color:#fbbf24;border-color:rgba(245,158,11,.45);font-weight:600">支持但未选 ✦</span>` : ""}
        ${mt === "selected" ? `<span class="badge" style="color:#4ade80;border-color:rgba(34,197,94,.4);font-weight:600">已选 ✓</span>` : ""}
        ${keyBadge(u)}
        ${netExitBadge(u)}
        ${u.api_mode === "responses"
          ? `<span class="badge blue" title="这个上游走 /v1/responses：请求体、响应体、流式事件都由代理转换">Responses 接口</span>`
          : ""}
        ${nAvail
          ? `<span class="badge ${nSel ? "blue" : "warn"}">已选用 ${nSel} / ${nAvail} 个模型</span>`
          : `<span class="badge warn">还没拉取模型</span>`}
        ${balanceBadge(u)}
        ${u.models_fetched_at ? `<span class="badge">${whenText(u.models_fetched_at)}拉取</span>` : ""}
      </div>
      <div class="kv">
        <div class="stat"><b>${u.requests}</b><span>请求</span></div>
        <div class="stat"><b style="color:var(--ok)">${u.successes}</b><span>成功</span></div>
        <div class="stat"><b style="color:${u.failures ? "var(--bad)" : "var(--dim)"}">${u.failures}</b><span>失败</span></div>
        <div class="stat"><b style="color:${latColor(lat)}">${fmtLat(lat)}</b><span>平均延迟</span></div>
      </div>
      ${lat ? `<div class="bar"><i style="width:${Math.max(4, Math.round(lat / maxLat * 100))}%;
              background:${latColor(lat)}"></i></div>` : ""}
      ${renderMirrors(u)}
      ${u.models_error ? `<div class="hint" style="color:var(--bad)">拉取失败：${esc(u.models_error)}</div>` : ""}
      ${u.last_error && !u.models_error ? `<div class="hint">最近错误：${esc(u.last_error)}</div>` : ""}
      ${renderCheckResults(u)}
      <div class="up-actions">
        <button class="mini primary" data-act="models" ${nAvail ? "" : "disabled"}>
          ${open ? "收起模型" : "选模型"}${nAvail ? ` (${nSel}/${nAvail})` : ""}
        </button>
        <button class="mini" data-act="fetch">拉取模型</button>
        <button class="mini" data-act="test">测速</button>
        <button class="mini" data-act="balance">查余额</button>
        <button class="mini" data-act="edit">编辑</button>
        <button class="mini" data-act="toggle">${u.enabled ? "停用" : "启用"}</button>
        <button class="mini danger" data-act="del">删除</button>
      </div>
      ${open ? renderPickPanel(u) : ""}
    </div>`;
  }).join("");
  if (applyHtml("upstreams", $("#upstreams"), html)) {
    restorePickScroll($("#upstreams"));
  }
}

// ---------------- 分组名下拉：可搜索、可选已有分组、也能新建 ----------------
//
// 已有分组从两条来源合并：
//   1. 别的上游已经起过的分组名（含还没建路由的）
//   2. 已经建好的模型路由键名
// 都收集起来去重，用起来就像个带搜索的输入框。

function allGroupNames() {
  const names = new Map();   // 名字 -> 提示信息
  (state.upstreams || []).forEach(u => {
    Object.entries(u.model_groups || {}).forEach(([real, g]) => {
      if (!g) return;
      if (!names.has(g)) names.set(g, { count: 0, isCurrent: false });
      names.get(g).count += 1;
    });
  });
  Object.keys(state.models || {}).forEach(g => {
    if (!names.has(g)) names.set(g, { count: 0, isCurrent: true });
    else names.get(g).isCurrent = true;
  });
  return names;
}

function closeGroupMenu() {
  const m = $("#groupMenu");
  if (m) { m.hidden = true; m.innerHTML = ""; }
  groupMenuFor = null;
}

let groupMenuFor = null;   // 当前展开下拉的那个输入框

function openGroupMenu(input) {
  const menu = $("#groupMenu");
  const groups = allGroupNames();
  // 默认值是模型名(还没改)时，不拿它当筛选词，否则一点开什么都搜不到
  const raw = input.value.trim();
  const typed = raw === (input.dataset.groupFor || "") ? "" : raw;
  const all = [...groups.keys()].sort((a, b) => a.localeCompare(b));
  const matched = typed
    ? all.filter(g => g.toLowerCase().includes(typed.toLowerCase()))
    : all;

  let html = "";
  if (!all.length) {
    html += `<div class="tip">还没有别的分组。直接在框里打一个新名字，按回车即可。</div>`;
  } else if (!matched.length) {
    html += `<div class="tip">没有匹配「${esc(typed)}」的分组</div>`;
  } else {
    html += matched.slice(0, 30).map(g => {
      const info = groups.get(g);
      const cur = g === (input.dataset.currentGroup || "");
      return `<div class="opt ${cur ? "cur" : ""}" data-pick="${esc(g)}">
        ${esc(g)}
        <span class="n">${info.count ? info.count + " 个上游" : ""}${
          info.isCurrent ? (info.count ? " · " : "") + "已有路由" : ""
        }</span>
      </div>`;
    }).join("");
  }
  // 输入了新名字 → 给一个新建选项
  if (typed && !all.includes(typed)) {
    html += `<div class="opt newopt" data-pick="${esc(typed)}">
      ＋ 新建分组「${esc(typed)}」</div>`;
  }
  // 清空映射，回到用真实名
  html += `<div class="opt" data-pick="" style="color:var(--dim)">
    用真实模型名（不分组）</div>`;

  menu.innerHTML = html;
  menu.hidden = false;
  groupMenuFor = input;
  positionGroupMenu(input);

  menu.querySelectorAll("[data-pick]").forEach(el => {
    el.onmousedown = ev => {
      ev.preventDefault();          // 别让输入框先失焦
      closeGroupMenu();
      saveGroup(input.dataset.upstream, input.dataset.groupFor, el.dataset.pick);
    };
  });
}

// 把菜单贴到输入框旁边；下方空间不够就往上翻
function positionGroupMenu(input) {
  const menu = $("#groupMenu");
  if (!input || !menu || menu.hidden) return;
  const r = input.getBoundingClientRect();
  const h = Math.min(menu.scrollHeight, 210);
  const below = window.innerHeight - r.bottom;
  const flipUp = below < h + 12 && r.top > h + 12;
  menu.style.left = Math.max(8, Math.min(r.left, window.innerWidth - menu.offsetWidth - 8)) + "px";
  menu.style.top = (flipUp ? r.top - h - 4 : r.bottom + 4) + "px";
}

// 每个上游展开后的模型面板：勾选要用的模型 + 给每个模型指定「归到哪个分组」
//
// 关键区别：
//   真实模型名（上游那边叫什么，如 ds1/ds2/ds3/ds4）—— 只读，来自拉取
//   分组名（客户端统一用什么名字调，如 deepseek）—— 你来选/填，一样的就归到一组
function renderPickPanel(u) {
  const avail = u.available_models || [];
  const selected = new Set(u.selected_models || []);
  const groups = u.model_groups || {};
  const q = (pickFilter[u.id] || "").toLowerCase();
  const shown = q ? avail.filter(m => m.toLowerCase().includes(q)) : avail;

  const rows = shown.map(m => {
    const on = selected.has(m);
    // 默认分组名就是模型名；用户改成别的才存映射。
    // mapped 为空 = 没有自定义映射（等价于用模型名），视觉上不点亮。
    const mapped = groups[m] || "";
    const g = mapped || m;
    return `
    <div class="pickrow ${on ? "on" : ""}">
      <label class="row" style="gap:8px;cursor:pointer;flex:1;min-width:0">
        <input type="checkbox" data-model="${esc(m)}" ${on ? "checked" : ""}>
        <span title="${esc(m)}">${esc(m)}</span>
      </label>
      ${on ? `<input class="groupbox mono ${mapped ? "has-group" : ""}"
                 data-group-for="${esc(m)}"
                 data-upstream="${esc(u.id)}" data-current-group="${esc(g)}"
                 placeholder="分组名 · 可搜索/新建" value="${esc(g)}"
                 title="默认就是模型名；填成别的名字就能和别的上游里的模型合成一个可调用的名字">` : ""}
    </div>`;
  }).join("");

  // 这个上游当前的分组情况
  const gmap = {};
  selected.forEach(m => {
    const g = groups[m] || m;
    (gmap[g] = gmap[g] || []).push(m);
  });
  const aliased = Object.entries(gmap).filter(([g, ms]) => ms.length > 1 || g !== ms[0]);

  return `
  <div class="pickpanel">
    <div class="pickpanel-head">
      <span class="hint">勾选要用的模型；右边填<b>分组名</b>，填成一样的就会合成一个名字</span>
      <div class="grow"></div>
      <input class="filterbox" data-act="pickfilter" placeholder="筛选模型…"
             value="${esc(pickFilter[u.id] || "")}">
      <button class="mini" data-act="pickall">全选</button>
      <button class="mini" data-act="picknone">全不选</button>
    </div>
    ${avail.length
      ? (shown.length
          ? `<div class="picklist">${rows}</div>`
          : `<div class="hint" style="padding:10px">没有匹配「${esc(q)}」的模型</div>`)
      : `<div class="hint" style="padding:10px">这个上游还没拉到模型，先点「拉取模型」</div>`}
    <div class="pickpanel-bar">
      <button class="mini primary" data-act="confirm-group" ${selected.size ? "" : "disabled"}
              title="把上面所有分组输入一次性写入，避免失焦/回车误触发">
        确认分组
      </button>
      <span class="hint">修改后点「确认分组」才生效；取消勾选已分组模型会保留映射，等下次勾选后再重新分组。</span>
    </div>
    <div class="hint panel-groups" style="margin-top:8px">
      已选 ${selected.size} 个。
      ${aliased.length
        ? `本上游的分组：` + aliased.map(([g, ms]) =>
            `<span class="badge purple mono">${esc(g)}</span> ← ${ms.map(esc).join(", ")}`).join("　")
        : ""}
    </div>
  </div>`;
}

// 事件委托：监听器挂在容器上，卡片被重建也不会丢事件
function bindDelegatedEvents() {
  $("#upstreams").addEventListener("click", e => {
    const btn = e.target.closest("button[data-act]");
    if (!btn || btn.disabled) return;
    const card = btn.closest(".up");
    if (!card) return;
    const id = card.dataset.id;
    const act = btn.dataset.act;
    if (act === "models") {
      expanded[id] = !expanded[id];
      renderUpstreams();
    }
    else if (act === "fetch") fetchModels([id]);
    else if (act === "test") testUpstream(id);
    else if (act === "balance") checkBalance(id);
    else if (act === "edit") editUpstream(id);
    else if (act === "pickall") selectModels(id, { all: true });
    else if (act === "picknone") selectModels(id, { none: true });
    else if (act === "confirm-group") commitGroupChanges(id);
    else if (act === "toggle") {
      const u = (state.upstreams || []).find(x => x.id === id);
      if (u) updateUpstream({ id, enabled: !u.enabled }, u.enabled ? "已停用 " : "已启用 ");
    } else if (act === "del") delUpstream(id);
  });

  // 勾选框：勾上/取消立刻保存
  $("#upstreams").addEventListener("change", e => {
    const cb = e.target.closest('input[type="checkbox"][data-model]');
    if (!cb) return;
    const card = cb.closest(".up");
    if (!card) return;
    const id = card.dataset.id;
    // 以界面上实际的勾选为准，而不是 state —— 连点多个模型时
    // state 还没刷新，用旧列表会把刚勾的冲掉
    const models = [...card.querySelectorAll('input[type="checkbox"][data-model]')]
      .filter(x => x.checked).map(x => x.dataset.model);
    selectModels(id, { models }, { keepPanel: true });
  });

  // 分组名输入框：只更新本地预览，不立刻提交；点「确认分组」才写服务端
  $("#upstreams").addEventListener("input", e => {
    const gb = e.target.closest("input[data-group-for]");
    if (!gb) return;
    updateLocalGroupPreview(gb);
  });
  $("#upstreams").addEventListener("blur", e => {
    const gb = e.target.closest("input[data-group-for]");
    if (!gb) return;
    updateLocalGroupPreview(gb);
  });
  $("#upstreams").addEventListener("keydown", e => {
    const gb = e.target.closest("input[data-group-for]");
    if (!gb) return;
    if (e.key === "Enter") {
      e.preventDefault();
      closeGroupMenu();
      // 选中候选或回车确认，只更新本地预览，不立刻提交
      updateLocalGroupPreview(gb);
      // 不 blur —— 让用户可以继续打字，或者点确认按钮
      return;
    }
    else if (e.key === "Escape") { closeGroupMenu(); gb.blur(); }
    // 上下键在候选里走动，回车选中
    else if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      const menu = $("#groupMenu");
      if (menu.hidden) { openGroupMenu(gb); return; }
      const opts = [...menu.querySelectorAll("[data-pick]")];
      if (!opts.length) return;
      const cur = opts.findIndex(o => o.classList.contains("hover"));
      const next = e.key === "ArrowDown"
        ? (cur + 1) % opts.length
        : (cur <= 0 ? opts.length - 1 : cur - 1);
      opts.forEach(o => o.classList.remove("hover"));
      opts[next].classList.add("hover");
      gb.value = opts[next].dataset.pick;
      // 候选选中也只做本地预览
      updateLocalGroupPreview(gb);
    }
  });
  // 点开/输入时弹出候选
  $("#upstreams").addEventListener("focusin", e => {
    const gb = e.target.closest("input[data-group-for]");
    if (gb) openGroupMenu(gb);
  });
  $("#upstreams").addEventListener("input", e => {
    const gb = e.target.closest("input[data-group-for]");
    if (gb) openGroupMenu(gb);
  });

  // 面板里的筛选框（只影响显示，不动数据）
  $("#upstreams").addEventListener("input", e => {
    const f = e.target.closest('input[data-act="pickfilter"]');
    if (!f) return;
    const card = f.closest(".up");
    if (!card) return;
    pickFilter[card.dataset.id] = f.value;
    const pos = f.selectionStart;
    renderUpstreams();
    // 重渲染后把光标放回去，否则输一个字符就失焦
    const again = document.querySelector(
      `.up[data-id="${card.dataset.id}"] input[data-act="pickfilter"]`);
    if (again) { again.focus(); try { again.setSelectionRange(pos, pos); } catch (err) {} }
  });

  $("#models").addEventListener("click", e => {
    const del = e.target.closest("[data-del]");
    if (del) {
      e.stopPropagation();
      deleteModelByName(del.dataset.del);
      return;
    }
    const card = e.target.closest(".mcard");
    if (card && card.dataset.name) openModel(card.dataset.name);
  });
}

/* ================= 渲染：模型 ================= */
let modelLimit = 200;
let selectedModel = null;

function renderModels() {
  const cat = state.catalog || [];
  const configured = state.models || {};
  const q = $("#mFilter").value.trim().toLowerCase();
  const mode = $("#mFilterMode").value;
  let list = cat.map(e => ({
    name: e.name,
    // 注意：后端字段叫 upstreams（已选用的），不是 ups
    ups: e.upstreams || [],
    offeredOn: e.offered_only || [],
    real_models: e.real_models || [],
    aliased: !!e.aliased,
    configured: !!configured[e.name],
    strategy: configured[e.name] ? configured[e.name].strategy : null,
    affinity: configured[e.name] ? (configured[e.name].affinity || "auto") : "auto",
    upstream_names: configured[e.name]
      ? configured[e.name].upstream_names
      : (e.upstreams || []).map(u => u.name),
  }));

  // 已配置但拉取列表里没有的（手工维护的），也补进来
  Object.keys(configured).forEach(name => {
    if (!list.some(x => x.name === name)) {
      list.push({ name, ups: [], offeredOn: [], real_models: configured[name].real_models || [],
                  aliased: (configured[name].real_models || []).some(r => r !== name),
                  configured: true,
                  strategy: configured[name].strategy,
                  affinity: configured[name].affinity || "auto",
                  upstream_names: configured[name].upstream_names });
    }
  });

  // 只显示「至少有一个上游选用」的模型 —— 没勾选的不进路由
  list = list.filter(x => x.ups.length || x.configured);

  list.sort((a, b) => {
    if (a.configured !== b.configured) return a.configured ? -1 : 1;
    if (b.ups.length !== a.ups.length) return b.ups.length - a.ups.length;
    return a.name.localeCompare(b.name);
  });

  if (q) list = list.filter(x => x.name.toLowerCase().includes(q));
  if (mode === "configured") list = list.filter(x => x.configured);
  else if (mode === "unconfigured") list = list.filter(x => !x.configured);
  else if (mode === "multi") list = list.filter(x => x.ups.length > 1);

  const nCfg = Object.keys(configured).length;
  const total = list.length;
  const page = list.slice(0, modelLimit);
  $("#mCount").textContent = `已选用 ${total} 个模型，已配置路由 ${nCfg} 个` +
    (total > page.length ? `（显示前 ${page.length} 个）` : "");

  if (!list.length) {
    const anyOffered = cat.length > 0;
    applyHtml("models", $("#models"), `<div class="empty">${
      anyOffered
        ? "还没有选用任何模型。<br>切到左侧「上游来源」标签页，点上游卡片上的<b>选模型</b>，勾上你要用的。"
        : "还没有模型信息。先添加上游，然后点「拉取全部模型」。"}</div>`);
    if (!editing) {
      const d = $("#modelDetail"), e = $("#modelEmpty");
      if (d) d.hidden = true;
      if (e) e.hidden = false;
    }
    return;
  }

  const STRAT = { priority: "按顺序", round_robin: "轮询", random: "随机" };
  const more = total > page.length
    ? `<div style="padding:12px 4px"><button class="mini" id="modelsMore">加载更多（还有 ${total - page.length} 个）</button></div>`
    : "";
  applyHtml("models", $("#models"), `<div class="mlist">` + page.map(m => {
    const has = m.ups.filter(u => u.enabled).length;
    const cls = m.configured ? "configured" : (m.ups.length ? "" : "none");
    const c = configured[m.name] || {};
    const members = c.members || [];
    const nSkip = members.filter(x => x.quota === 0).length;
    const nLimited = members.filter(x => x.quota != null && x.quota > 0).length;
    // 展示每个上游用的真实名字，这样「同名分组、各家不同名」一眼可见
    const routeNames = m.upstream_names || (m.ups || []).map(u => u.name);
    const order = routeNames.length
      ? routeNames.map((n, i) => {
          const real = (m.real_models || [])[i];
          return real && real !== m.name ? `${n}(${real})` : n;
        }).join(" → ")
      : "（没有上游选用它）";
    const routeChips = routeNames.slice(0, 4).map((n, i) => {
      const real = (m.real_models || [])[i];
      const title = real && real !== m.name ? `${n}（${real}）` : n;
      return `<span class="route-chip" title="${esc(title)}">${esc(n)}</span>`;
    }).join('<span class="route-arrow">→</span>');
    const routeExtra = routeNames.length > 4
      ? `<span class="route-more">+${routeNames.length - 4}</span>` : "";
    const hue = hueOf(m.name);
    return `
    <div class="mcard ${cls}${selectedModel === m.name ? " active" : ""}" data-name="${esc(m.name)}" style="--h:${hue}">
      <div class="mcard-top">
        <span class="mcard-avatar" aria-hidden="true">${esc(initialOf(m.name))}</span>
        <div class="mcard-title">
          <div class="mcard-name" title="${esc(m.name)}">${esc(m.name)}</div>
          <div class="mcard-sub">
            ${m.configured
              ? `<span class="badge ok">已配置 · ${STRAT[m.strategy] || m.strategy}</span>`
              : `<span class="badge">未配置</span>`}
            ${m.ups.length ? `<span class="badge blue">${m.ups.length} 个上游选用</span>`
                           : `<span class="badge warn">无上游选用</span>`}
            ${m.aliased ? `<span class="badge purple">各家名字不同</span>` : ""}
            ${m.configured && c.active_ordering
              ? `<span class="badge purple" title="当前生效的具名排序">排序：${esc(c.active_ordering)}</span>`
              : ""}
            ${m.configured && m.affinity && m.affinity !== "auto"
              ? `<span class="badge purple" title="会话粘性策略">粘性：${esc(m.affinity)}</span>`
              : ""}
            ${nSkip ? `<span class="badge bad" title="额度为 0，调用时会跳过">${nSkip} 个不调用</span>` : ""}
            ${nLimited ? `<span class="badge warn" title="设了调用额度上限">${nLimited} 个限额度</span>` : ""}
            ${m.ups.length > has && m.configured ? `<span class="badge warn">${m.ups.length - has} 个已停用</span>` : ""}
          </div>
        </div>
        <span class="mcard-count" title="参与这个分组的模型数量">${m.ups.length}</span>
      </div>
      <div class="mcard-route" title="${esc(order)}">${routeChips || `<span class="route-more">（没有上游选用它）</span>`}${routeExtra}</div>
      <div class="mcard-hint">${m.configured ? "点击调整上游顺序" : "点击配置路由"}</div>
      ${m.configured
        ? `<button class="mini danger mcard-del" data-del="${esc(m.name)}"
             title="删除这个模型的路由配置(删除后会退回自动模式)">删除路由</button>`
        : ""}
    </div>`;
  }).join("") + more + `</div>`);
  const sideCount = $("#mSideCount");
  if (sideCount) sideCount.textContent = `${total} 个`;
  const moreBtn = $("#modelsMore");
  if (moreBtn) moreBtn.onclick = () => { modelLimit += 200; renderModels(); };
  if (!editing && page.length) openModel(page[0].name);
  else paintModelSelection();
}

function paintModelSelection() {
  document.querySelectorAll("#models .mcard").forEach(el => {
    el.classList.toggle("active", el.dataset.name === selectedModel);
  });
  const detail = $("#modelDetail"), empty = $("#modelEmpty");
  if (detail) detail.hidden = !editing;
  if (empty) empty.hidden = !!editing;
}

/* ================= 模型编辑 ================= */
const STRAT_HINT = {
  priority: "按顺序依次调用：永远先用排在最上面的上游，它挂了（或限流、额度用尽）才用下一个。",
  round_robin: "轮流轮询：请求 1 走第一个，请求 2 走第二个……转一圈再从头开始。适合多个账号分摊额度。",
  random: "随机挑一个：每次从可用上游里随机选一个。",
};

function openModel(name) {
  selectedModel = name;
  const cfg = (state.models || {})[name];
  const catEntry = (state.catalog || []).find(e => e.name === name);
  const catalogUps = catEntry ? catEntry.upstreams : [];

  let picks;
  if (cfg && (cfg.entries || []).length) {
    picks = cfg.entries.map(e => {
      const u = (state.upstreams || []).find(x => x.id === e.upstream);
      return {
        id: e.upstream,
        name: u ? u.name : e.upstream,
        hasModel: u ? (u.available_models || []).includes(e.model || name) : false,
        model: e.model || name,
        enabled: e.enabled !== false,
        quota: e.quota != null ? e.quota : "",
      };
    });
  } else {
    // 没配过：把参与这个分组的上游都放进来，用它们各自的真实名字
    picks = catalogUps.map(u => ({
      id: u.id, name: u.name, hasModel: true,
      model: u.model || name, enabled: true, quota: "",
    }));
  }

  // 具名排序：直接用 stats 里给的数据（含「当前顺序对应哪一套」）
  const orderings = (cfg && cfg.orderings) ? Object.keys(cfg.orderings) : [];
  editing = {
    name,
    strategy: cfg ? cfg.strategy : (state.default_strategy || "priority"),
    affinity: cfg ? (cfg.affinity || "auto") : "auto",
    picks,
    orderings,
    activeOrdering: cfg ? (cfg.active_ordering || "") : "",
    sortLabel: "",
  };
  renderModal();
  paintModelSelection();
  const detail = $("#modelDetail"), empty = $("#modelEmpty");
  if (detail) detail.hidden = false;
  if (empty) empty.hidden = true;
}

function toggleCollapsible(btn) {
  const body = btn.closest(".collapsible").querySelector(".collapsible-body");
  const expanded = btn.getAttribute("aria-expanded") === "true";
  btn.setAttribute("aria-expanded", String(!expanded));
  body.hidden = expanded;
}

// 一行「测试」按钮的结果：可用/不可用/限流 + 实测延迟，测的是这一行的那个模型
// 旧流水只有状态码时补充可能原因；已带上游详情的新记录原样显示。
function explainHttpError(reason, status) {
  const text = String(reason || "");
  const match = text.match(/^(?:客户端错误 )?HTTP (\d+)$/);
  if (text && !match) return text;
  const code = Number(status || (match && match[1]));
  if (!(code >= 400)) return text;
  const hint = (state && state.http_error_hints || {})[code];
  return `HTTP ${code}：${hint || "上游请求失败；旧记录未保存具体原因"}`;
}

function pickTestBadge(p) {
  return checkBadge(p && p.test);
}

function requestPathSummary(h, trail) {
  const recovered = !!h.ok && trail.some(s => !s.ok);
  const outcome = recovered ? "切换后成功" : h.ok ? "成功" : "最终失败";
  const tone = recovered ? "warn" : h.ok ? "ok" : "bad";
  const shortErrors = {400:"参数/上下文错误",401:"认证失败",402:"付费/额度限制",
    403:"权限/访问限制",404:"接口/模型不存在",405:"请求方法错误",408:"请求超时",
    409:"请求冲突",413:"请求过大",415:"格式不支持",422:"参数不兼容",
    425:"暂不接受请求",429:"限流/配额限制",500:"上游内部错误",502:"网关错误",
    503:"暂不可用",504:"网关超时",520:"源站响应异常",521:"源站拒绝连接",
    522:"连接超时",523:"源站不可达",524:"源站响应超时"};
  // 没有尝试链的旧记录只知道最后落点，不能从候选数量推断中间路径。
  const hops = trail.length ? trail : [{upstream:h.upstream,ok:h.ok,status:h.status,reason:h.error}];
  const nodes = hops.map(s => {
    const code = s.status != null ? String(s.status) : "";
    const reason = String(s.reason || "");
    const brief = s.ok ? "成功" : /连接失败/.test(reason) ? "连接失败"
      : /流.*(中断|失败|断开)|空流/.test(reason) ? "流式响应异常"
      : Number(s.status) < 400 ? "响应异常" : shortErrors[s.status] || "调用失败";
    return `<span class="req-hop ${s.ok ? "good" : "bad"}"><span>${s.ok ? "✓" : "✗"}</span><span class="req-hop-name">${esc(s.upstream || "未知上游")}</span><span>${esc(code)} ${brief}</span></span>`;
  }).join('<span class="req-arrow" aria-hidden="true">→</span>');
  return `<div class="req-path" aria-label="实际调用路径"><span class="badge ${tone}">${outcome}</span>${nodes}${!trail.length ? '<span class="hint">仅记录最终落点，完整路径未知</span>' : ""}</div>`;
}

function requestAttemptLabel(h, trail) {
  if (trail.length) return `实际尝试 ${trail.length} 次`;
  if (h.attempts === 0) return "未尝试上游";
  return "实际尝试次数未知";
}

const detailedTests = {};
function detailedTestReport(p, model) {
  const t = detailedTests[`${p.id}|${model}`];
  if (!t) return "";
  if (t.running) return '<div class="hint" style="flex-basis:100%" role="status">详细测试中：短请求、长上下文、流式、工具调用及结果回传…</div>';
  if (t.error) return `<div class="hint" style="flex-basis:100%;color:var(--bad)">${esc(t.error)}</div>`;
  const checks = t.checks || [];
  return `<details open style="flex-basis:100%;min-width:0;overflow-wrap:anywhere">
    <summary class="hint">详细测试：${checks.filter(c => c.ok).length}/${checks.length} 项通过 · ${esc(whenText(t.at))} · 合成上下文 ${fmtNum(t.context_chars)} 字符</summary>
    ${checks.map(c => `<div style="margin-top:8px"><span class="badge ${c.skipped ? "warn" : c.ok ? "ok" : "bad"}">${esc(c.name)}：${c.skipped ? "跳过" : c.ok ? "成功" : "失败"}</span>
      <span class="hint">${c.status != null ? "HTTP " + esc(c.status) + " · " : ""}${c.elapsed_seconds != null ? "总耗时 " + fmtLat(c.elapsed_seconds) : ""}${c.first_chunk_seconds != null ? " · 首块 " + fmtLat(c.first_chunk_seconds) : ""}${c.request_bytes != null ? " · 请求 " + fmtNum(c.request_bytes) + " B" : ""}${c.chunks != null ? " · " + c.chunks + " 个有效块" : ""}</span>
      ${c.error ? `<div style="color:var(--bad)">${esc(c.error)}</div>` : ""}
      ${c.response_preview ? `<div class="hint">回复：${esc(c.response_preview)}</div>` : ""}
    </div>`).join("")}
  </details>`;
}
function renderModal() {
  if (!editing) return;
  const { name, strategy, picks } = editing;

  $("#mTitle").textContent = name;
  const catEntry = (state.catalog || []).find(e => e.name === name);
  const nUp = catEntry ? catEntry.upstreams.length : picks.length;
  const aliased = picks.filter(p => p.model && p.model !== name);
  $("#mSubtitle").innerHTML = nUp
    ? `客户端统一用 <b class="mono">${esc(name)}</b> 调用，下面 ${nUp} 个上游按顺序试` +
      (aliased.length
        ? `<br>注意：${aliased.length} 个上游那边模型名不同，已各自标注`
        : "")
    : "还没有上游参与这个分组，可以手动把上游加进来";

  document.querySelectorAll("#mStrategy button").forEach(b =>
    b.classList.toggle("active", b.dataset.s === strategy));
  $("#mStrategyHint").textContent = STRAT_HINT[strategy] || "";
  if ($("#mAffinity")) $("#mAffinity").value = editing.affinity || "auto";
  if ($("#mAffinityHint")) {
    const am = editing.affinity || "auto";
    $("#mAffinityHint").textContent = am === "auto"
      ? "默认：有缓存收益才固定"
      : am === "off" ? "完全不固定" : am === "strict" ? "只认上次成功的上游" : "按会话/单轮固定";
  }

  if (!picks.length) {
    $("#mPicks").innerHTML = `<div class="empty" style="padding:18px">
      还没有选任何上游。用下面的下拉框加一个。</div>`;
  } else {
    $("#mPicks").innerHTML = picks.map((p, i) => `
      <div class="pick" data-idx="${i}">
        <span class="handle" title="按住拖动调整顺序">⠿</span>
        <span class="seqnum">${i + 1}</span>
        <span class="pick-name">${esc(p.name)}</span>
        <div class="pick-actions">
          <div class="pick-move">
            <button class="mini" data-act="up" title="上移一位" ${i === 0 ? "disabled" : ""}>↑</button>
            <button class="mini" data-act="down" title="下移一位" ${i === picks.length - 1 ? "disabled" : ""}>↓</button>
          </div>
          ${p.model && p.model !== name
            ? `<span class="badge purple mono" title="这个上游那边的真实模型名">${esc(p.model)}</span>`
            : `<span class="badge blue mono">${esc(p.model || name)}</span>`}
          <label class="hint row" style="gap:4px;cursor:pointer">
            <input type="checkbox" data-act="enable" ${p.enabled ? "checked" : ""}> 参与
          </label>
          <label class="quota-cell" title="留空 = 不限量；0 = 这家不调用（调用时跳过、排序时沉到最后）">
            <input type="number" min="0" step="1" data-quota="${i}"
                   placeholder="不限量" value="${p.quota === "" || p.quota == null ? "" : p.quota}">
            ${p.quota === 0 ? `<span class="badge bad">不调用</span>` : ""}
          </label>
          <button class="mini" data-act="test" data-idx="${i}"
                  title="测试该模型的短请求，并提示最近实际调用错误；不验证长上下文或流式兼容性">测试</button>
          ${pickTestBadge(p)}
          <button class="mini" data-act="detailed-test" data-idx="${i}" ${detailedTests[`${p.id}|${p.model || name}`]?.running ? "disabled" : ""}>详细测试</button>
          <button class="mini danger" data-act="rm">移除</button>
        </div>
        ${detailedTestReport(p, p.model || name)}
      </div>`).join("");
  }

  renderPreview();
  renderSortBox();
  renderQuotaHint();

  // 可添加的上游
  const used = new Set(picks.map(p => p.id));
  const avail = (state.upstreams || []).filter(u => !used.has(u.id));
  $("#mAddUpstream").innerHTML = avail.length
    ? avail.map(u => `<option value="${esc(u.id)}">${esc(u.name)}${
        (u.available_models || []).includes(name) ? " ✓有此模型" : ""}</option>`).join("")
    : `<option value="">（所有上游都已经在列表里）</option>`;
  $("#mAddBtn").disabled = !avail.length;

  bindModal();
}

// 顺序预览（带上各家真实名字，方便确认映射对不对）。
// 额度为 0 的会被跳过，不能算进「首选」，否则预览会骗人。
// 单独拎出来是因为改额度时要即时更新它，又不能重建整个弹窗（会打断输入）。
function renderPreview() {
  if (!editing) return;
  const { name, strategy, picks } = editing;
  const active = picks.filter(p => p.enabled && p.quota !== 0);
  const zeroed = picks.filter(p => p.enabled && p.quota === 0);
  const skippedTag = zeroed.length
    ? `<span class="hint">（额度 0 跳过：${zeroed.map(p => esc(p.name)).join("、")}）</span>`
    : "";
  if (active.length) {
    $("#mPreview").innerHTML = `<span class="hint">${
      strategy === "round_robin" ? "轮询顺序：" : "首选："}</span>` +
      active.map((p, i) =>
        `<span class="t ${i === 0 ? "first" : ""}">${esc(p.name)}${
          p.model && p.model !== name ? `<span class="muted"> · ${esc(p.model)}</span>` : ""
        }</span>`).join("") +
      (active.length > 1 ? `<span class="hint">（失败时依次向后重试）</span>` : "") +
      skippedTag;
  } else if (zeroed.length) {
    $("#mPreview").innerHTML = `<span class="hint" style="color:var(--warn)">` +
      `所有参与的上游额度都是 0，这个分组没有上游可用（会退回「自动」模式）。` +
      `把至少一个的额度清空或改成大于 0 才行。</span>`;
  } else {
    $("#mPreview").innerHTML = `<span class="hint" style="color:var(--warn)">没有选中任何上游，这个分组会走「自动」模式</span>`;
  }
}

// 具名排序那一块：下拉列已有的，按钮做保存/切换/删除，以及「按测速重排」
function renderSortBox() {
  const labels = editing.orderings || [];
  const sel = $("#mSortSel");
  const cur = editing.sortLabel || "";
  sel.innerHTML = (labels.length
    ? labels.map(l => `<option value="${esc(l)}">${esc(l)}</option>`).join("")
    : `<option value="">（还没有保存过排序）</option>`);
  // 优先显示当前生效的那套；否则保留用户刚选的
  const want = labels.includes(cur) ? cur
    : (labels.includes(editing.activeOrdering) ? editing.activeOrdering
       : (labels[0] || ""));
  sel.value = want;
  editing.sortLabel = want;
  sel.disabled = !labels.length;
  $("#mSortApply").disabled = !labels.length;
  $("#mSortDelete").disabled = !labels.length || want === "原始";

  $("#mSortHint").innerHTML = labels.length
    ? (editing.activeOrdering === "原始"
        ? `当前就是<b>原始顺序</b>（第一次保存排序时自动留的底）。`
        : (editing.activeOrdering
            ? `当前顺序 = <b>${esc(editing.activeOrdering)}</b>。拖动排序或改额度后，点「保存当前顺序为…」存成新的；不想动原始顺序时先切回「原始」。`
            : `当前顺序没存过。要留住就点「保存当前顺序为…」起个名字。`))
    : `想把「按顺序依次调用」的家数排得更合理？先点「⚡ 按测速重排并保存」，` +
      `会自动把快的排前面，并把那次结果存成一套叫「按测速」的排序，原始顺序不会丢。`;
}

// 把弹窗里的额度输入收进 picks（保存前调用）
function collectQuotas() {
  document.querySelectorAll("#mPicks input[data-quota]").forEach(inp => {
    const i = Number(inp.dataset.quota);
    const p = editing.picks[i];
    if (!p) return;
    const raw = (inp.value || "").trim();
    p.quota = raw === "" ? "" : Math.max(0, Math.floor(Number(raw) || 0));
  });
}

function bindModal() {
  document.querySelectorAll("#mPicks .pick").forEach(el => {
    const i = Number(el.dataset.idx);
    el.querySelector('[data-act="rm"]').onclick = () => {
      editing.picks.splice(i, 1); renderModal();
    };
    el.querySelector('[data-act="enable"]').onchange = e => {
      editing.picks[i].enabled = e.target.checked; renderModal();
    };
    // 手机上的兜底：行内 ↑↓ 直接换相邻两行，不依赖拖拽手势
    el.querySelector('[data-act="up"]').onclick = () => movePick(i, i - 1);
    el.querySelector('[data-act="down"]').onclick = () => movePick(i, i + 1);
  });

  // 「测试」按钮：精确测这一行的那个模型
  document.querySelectorAll('#mPicks button[data-act="test"]').forEach(btn => {
    btn.onclick = () => testPick(Number(btn.dataset.idx), btn);
  });
  document.querySelectorAll('#mPicks button[data-act="detailed-test"]').forEach(btn => {
    btn.onclick = () => testPickDetailed(Number(btn.dataset.idx));
  });

  // 额度输入：改完（回车或失焦）就记进 editing，顺带更新「不调用」标记
  document.querySelectorAll("#mPicks input[data-quota]").forEach(inp => {
    const commit = () => {
      collectQuotas();
      const i = Number(inp.dataset.quota);
      const p = editing.picks[i];
      const cell = inp.closest(".quota-cell");
      if (p && p.quota === 0) {
        // 就地加/去「不调用」标记，避免整个弹窗重建打断输入
        if (cell && !cell.querySelector(".badge.bad")) {
          const tag = document.createElement("span");
          tag.className = "badge bad";
          tag.textContent = "不调用";
          cell.appendChild(tag);
        }
      } else {
        const tag = cell && cell.querySelector(".badge.bad");
        if (tag) tag.remove();
      }
      renderQuotaHint();
      renderPreview();          // 额度 0 就不该再算进「首选」
    };
    inp.addEventListener("change", commit);
    inp.addEventListener("keydown", e => { if (e.key === "Enter") { commit(); inp.blur(); } });
  });

}

function renderQuotaHint() {
  const picks = editing.picks || [];
  const n0 = picks.filter(p => p.quota === 0).length;
  const nSet = picks.filter(p => p.quota !== "" && p.quota != null && p.quota !== 0).length;
  $("#mQuotaHint").innerHTML = (n0 || nSet)
    ? `其中 ${n0} 个上游设为 0（不调用）${nSet ? `，${nSet} 个设了调用额度` : ""}。
       额度为 0 的上游调用时会被直接跳过，排序时也放在最后。`
    : `不填就代表不限量。填 0 表示这家不用。`;
}
/* ================= 拖动排序（鼠标 + 触屏） =================
   旧实现用 HTML5 的 dragstart/dragover/drop —— 触屏浏览器根本不发这些事件，
   所以手机上怎么拖都没反应。现在按输入方式分两条路：

   - 鼠标 / 触控笔：按在行上（按钮、输入框除外）拖过 3px 就开始，桌面手感不变
   - 触屏：按 ⠿ 手柄直接拖；按行里别处则长按 220ms 再进入拖动 ——
     不然手指一划就被浏览器当成滚动列表了

   两条路都只是把「按下 → 跟手 → 松手」翻译成一次 movePick(from, to)，
   落位时改 editing.picks 再重渲染，和行里的 ↑↓ 按钮是同一段逻辑。 */

let pickDrag = null;           // 当前这一轮拖动（等长按的等待期也在里面，看 mode）
const PICK_LONG_PRESS = 220;   // 触屏长按多久算「我要拖」
const PICK_SLOP = 10;          // 触屏位移超过这么多 = 用户其实在滚列表
const PICK_MOUSE_SLOP = 3;     // 鼠标抖动容忍

// 把 picks 里的行从 from 挪到 to。行内 ↑↓ 和拖动落位都走这里
function movePick(from, to) {
  const picks = editing && editing.picks;
  if (!picks) return;
  if (from < 0 || to < 0 || from >= picks.length || to >= picks.length || from === to) return;
  collectQuotas();               // 先把输入框里没提交的额度收进来，免得顺序变了配错行
  picks.splice(to, 0, picks.splice(from, 1)[0]);
  renderModal();
}

function pickRows() {
  return [...document.querySelectorAll("#mPicks .pick")];
}

// 找指针底下那一行（不在行里就返回 null）
function pickRowAt(target) {
  const el = target && target.closest ? target.closest("#mPicks .pick") : null;
  return el && el.isConnected ? el : null;
}

// 点在按钮/输入框/复选框上是要操作，不是要排序
function pickIsControl(target) {
  return !!(target && target.closest &&
            target.closest("button, input, select, textarea, label, a, details"));
}

// 行的位置只量一次（拖动还没开始、没有任何 transform 时）：
// 之后所有换算都基于它。不能每次移动都实时读 rect ——
// 让位的行正带着 transform 和过渡动画，读到的是动画中间态，落点会来回跳。
function pickMeasureBase(d) {
  d.baseMid = d.others.map(row => {
    const b = row.getBoundingClientRect();
    return (b.top + b.bottom) / 2;
  });
}

// 指针落在哪一格：只看「除自己以外的行」，越过谁的中线就插在谁前面。
// 这么算不用管自己那行（它正跟着手指跑），也就不会自己推自己。
function pickSlot(clientY) {
  const d = pickDrag;
  if (!d || !d.baseMid) return 0;
  // 容器滚动后，行在屏幕上的位置整体上移了 scrollDelta，先补回来
  const y = clientY + d.scrollDelta;
  let slot = 0;
  while (slot < d.baseMid.length && y > d.baseMid[slot]) slot++;
  return slot;
}

// 从元素往上找滚动容器（弹窗本身）。.mask 是 overflow:auto，滚的就是它
function pickScrollBox(el) {
  let p = el && el.parentElement;
  while (p && p !== document.body && p !== document.documentElement) {
    const st = getComputedStyle(p);
    if (/(auto|scroll)/.test(st.overflowY)) return p;
    p = p.parentElement;
  }
  return document.scrollingElement || document.documentElement;
}

// 开始跟手拖动。clientY 只作为起点记下来，之后的位移都相对它算
function pickBegin(el, clientY, touchId) {
  if (!editing || pickDrag) return;
  const rows = pickRows();
  const idx = rows.indexOf(el);
  if (idx < 0) return;
  const rect = el.getBoundingClientRect();
  const next = rows[idx + 1];
  const prev = rows[idx - 1];
  // 一格的高度 = 行高 + 行距（margin-bottom）。末行没有下一行，就问上一行。
  const ref = next || prev;
  const gap = ref
    ? Math.abs((next ? next.getBoundingClientRect().top - rect.bottom
                       : rect.top - prev.getBoundingClientRect().bottom))
    : 0;
  const sliceH = rect.height + Math.max(0, gap);
  pickDrag = {
    mode: "active", el, idx, rows, touchId,
    others: rows.filter(r => r !== el),
    shiftOf: new Map(),
    startY: clientY, autoY: clientY,
    height: sliceH, scrollBox: pickScrollBox(el),
    scrollDelta: 0, moved: false, target: idx, raf: 0,
  };
  pickMeasureBase(pickDrag);
  el.classList.remove("pressing");
  el.classList.add("dragging");
  document.body.classList.add("pick-dragging");
  el.style.width = rect.width + "px";
  startAutoScroll();
}

// 拖动中：行贴着手走，其余行让位
function pickDragTo(clientY) {
  const d = pickDrag;
  if (!d || d.mode !== "active") return;
  d.autoY = clientY;
  const dy = clientY - d.startY + d.scrollDelta;
  if (!d.moved) {
    if (Math.abs(dy) < 1) return;
    d.moved = true;
  }
  d.el.style.transform = `translateY(${dy}px)`;
  const slot = pickSlot(clientY);
  d.target = slot;
  // 让位方向：把自己那行从列表里抽走后，其余行是紧凑的；
  // 最终插在下标 slot 处，意味着所有 j >= slot 的行要往下让一格，
  // 而原本排在拖动行后面的（j >= idx）本来就要往上补一格。
  d.others.forEach((row, j) => {
    let shift = 0;
    if (j >= slot) shift += d.height;
    if (j >= d.idx) shift -= d.height;
    if ((d.shiftOf.get(row) || 0) !== shift) {
      d.shiftOf.set(row, shift);
      row.classList.add("shifting");
      row.style.transform = shift ? `translateY(${shift}px)` : "";
    }
  });
}

// 指针停在上下边缘时要一直滚，所以开个 rAF 循环盯着 ——
// 手停住不动就没有新事件，光靠 move 事年滚不起来。
function startAutoScroll() {
  const d = pickDrag;
  if (!d || d.raf) return;
  const step = () => {
    const cur = pickDrag;
    if (!cur || cur.mode !== "active") return;
    const box = cur.scrollBox;
    if (box && box.scrollHeight > box.clientHeight + 1) {
      const r = box.getBoundingClientRect();
      const band = Math.min(72, r.height / 4);
      const y = cur.autoY;
      let dy = 0;
      if (y < r.top + band) dy = -Math.ceil((r.top + band - y) / 5);
      else if (y > r.bottom - band) dy = Math.ceil((y - (r.bottom - band)) / 5);
      if (dy) {
        const before = box.scrollTop;
        box.scrollTop = before + dy;
        const real = box.scrollTop - before;
        if (real) cur.scrollDelta += real;   // 容器滚了，补偿起点，行才继续贴着手
      }
    }
    cur.raf = requestAnimationFrame(step);
  };
  d.raf = requestAnimationFrame(step);
}

// 松手：收尾 + 落位
function pickFinish() {
  const d = pickDrag;
  if (!d || d.mode !== "active") { pickAbort(); return; }
  const moved = d.moved && d.target !== d.idx;
  const from = d.idx, to = d.target;
  pickTeardown(d, d.moved);
  if (moved) movePick(from, to);
}

// 放弃这一轮：长按还没到点就在滚列表、来了第二根手指、系统打断手势。
// 这里只是把界面还原，不动顺序 —— 手势被系统抢走时用户多半是在滚动页面，
// 不该顺手改掉他的路由顺序。
function pickAbort() {
  const d = pickDrag;
  if (!d) return;
  if (d.timer) clearTimeout(d.timer);
  if (d.mode === "active") pickTeardown(d, false);
  else {
    if (d.el) d.el.classList.remove("pressing");
    pickDrag = null;
  }
}

function pickTeardown(d, swallowClick) {
  pickDrag = null;
  if (d.raf) cancelAnimationFrame(d.raf);
  if (d.el) {
    d.el.classList.remove("dragging", "pressing");
    d.el.style.transform = "";
    d.el.style.width = "";
  }
  document.body.classList.remove("pick-dragging");
  (d.rows || []).forEach(row => {
    row.classList.remove("shifting", "pressing");
    row.style.transform = "";
  });
  // 真拖过一段距离的话，抬手时浏览器还会补一个 click：
  // 手落在空白处会点到遮罩上把弹窗关掉，落在「移除」上会误删。
  // 所以这一下不要了，只管界面，不认动作。
  if (swallowClick) pickSwallowClickUntil = Date.now() + 400;
}
let pickSwallowClickUntil = 0;

// 拖动刚结束时的 click 在捕获阶段吃掉 —— 赶在关弹窗那类处理器前面。
// 只管弹窗遮罩和这几行里的 click，别处（比如「保存」）照旧。
document.addEventListener("click", e => {
  if (Date.now() > pickSwallowClickUntil) return;
  if (!e.target.closest("#mPicks")) return;
  e.stopPropagation();
  e.preventDefault();
}, true);

/* ---- 触屏：走 touch 事件。只有 touchmove 能 preventDefault 明确告诉浏览器
        「这一串移动归我」，所以触屏这条路不用 pointer。
        监听挂在弹窗上而不是 document 上：非 passive 的 touchmove 会让浏览器
        每次滚动都等 JS 一句话，只让弹窗里的滚动付这个代价。---- */
const pickModal = document.getElementById("modelDetail");

pickModal.addEventListener("touchstart", e => {
  if (!editing || pickDrag || e.touches.length !== 1) return;
  if (!editing) return;
  const el = pickRowAt(e.target);
  if (!el || pickIsControl(e.target)) return;
  const t = e.touches[0];
  if (e.target.closest(".handle")) {
    e.preventDefault();                 // 手柄按下去就是要拖，别让浏览器再想别的
    pickBegin(el, t.clientY, t.identifier);
    return;
  }
  // 行里别处：先等长按。轻轻一划应该是滚列表，不能一按就抢
  const pending = { mode: "pending", el, startX: t.clientX, startY: t.clientY,
                    touchId: t.identifier, timer: 0 };
  pickDrag = pending;
  el.classList.add("pressing");
  const startY = t.clientY, touchId = t.identifier;
  pending.timer = setTimeout(() => {
    if (pickDrag !== pending) return;
    pickDrag = null;
    if (navigator.vibrate) { try { navigator.vibrate(12); } catch (_) {} }
    pickBegin(el, startY, touchId);
  }, PICK_LONG_PRESS);
}, { passive: false });

pickModal.addEventListener("touchmove", e => {
  const d = pickDrag;
  if (!d) return;
  const t = [...e.touches].find(x => x.identifier === d.touchId);
  if (!t) return;
  if (d.mode === "pending") {
    if (Math.hypot(t.clientX - d.startX, t.clientY - d.startY) > PICK_SLOP) pickAbort();
    return;
  }
  e.preventDefault();                   // 已经在拖了，这段移动不许再拿去滚列表
  pickDragTo(t.clientY);
}, { passive: false });

pickModal.addEventListener("touchend", e => {
  const d = pickDrag;
  if (!d) return;
  if (d.mode === "pending") { pickAbort(); return; }
  // 那根手指抬起来了才算结束；还有别的手指按着就继续
  if (![...e.touches].some(x => x.identifier === d.touchId)) pickFinish();
});
pickModal.addEventListener("touchcancel", () => pickAbort());

/* ---- 鼠标 / 触控笔：pointer 事件，没有滚动冲突，按住拖过阈值就开始 ---- */
document.addEventListener("pointerdown", e => {
  if (!editing || pickDrag) return;
  if (e.pointerType === "touch") return;       // 触屏归上面那套管
  if (e.button !== 0) return;
  if (!editing) return;
  const el = pickRowAt(e.target);
  if (!el || pickIsControl(e.target)) return;
  pickDrag = { mode: "armed", el, startX: e.clientX, startY: e.clientY,
               pointerId: e.pointerId };
});

document.addEventListener("pointermove", e => {
  const d = pickDrag;
  if (!d || d.pointerId !== e.pointerId) return;
  if (d.mode === "armed") {
    if (Math.hypot(e.clientX - d.startX, e.clientY - d.startY) < PICK_MOUSE_SLOP) return;
    const el = d.el;
    const pid = e.pointerId;
    pickDrag = null;
    pickBegin(el, e.clientY, null);
    if (pickDrag) {
      pickDrag.pointerId = pid;
      // 抓住指针：拖到窗口外松手也能收到 pointerup，不会留一个卡住的状态
      try { el.setPointerCapture(pid); } catch (_) {}
    }
    return;
  }
  if (d.mode === "active") pickDragTo(e.clientY);
});

document.addEventListener("pointerup", e => {
  const d = pickDrag;
  if (!d || d.pointerId !== e.pointerId) return;
  if (d.mode === "armed") { pickDrag = null; return; }    // 只是点了一下
  try { d.el.releasePointerCapture(e.pointerId); } catch (_) {}
  pickFinish();
});

document.addEventListener("pointercancel", e => {
  const d = pickDrag;
  if (d && d.pointerId === e.pointerId) pickAbort();
});

/* ================= 操作 ================= */
// 测试某一行(某上游 + 它的那个真实模型)的连通性。
// 不重建整个弹窗 —— 只更新这一行按钮旁的结果，免得打断输入。
async function testPickDetailed(i) {
  if (!editing) return;
  const group = editing;
  const p = group.picks[i];
  if (!p) return;
  const model = p.model || group.name;
  const key = `${p.id}|${model}`;
  if (detailedTests[key]?.running) return;
  collectQuotas();
  detailedTests[key] = { running: true };
  renderModal();
  try {
    const r = await api("/admin/speedtest", { detailed: true, tests: [{ id: p.id, model }] });
    const t = (r.results || [])[0];
    if (!t || !Array.isArray(t.checks)) throw new Error("没有返回详细测试结果");
    detailedTests[key] = t;
    toast(`${p.name} · ${model} 详细测试：${t.checks.filter(c => c.ok).length}/${t.checks.length} 项通过`, t.ok ? "ok" : "warn");
  } catch (e) {
    detailedTests[key] = { error: "详细测试失败：" + e.message };
    toast(detailedTests[key].error, "bad");
  }
  if (editing === group) { collectQuotas(); renderModal(); }
}
async function testPick(i, btn) {
  if (!editing) return;
  const p = editing.picks[i];
  if (!p) return;
  const model = p.model || editing.name;
  const old = btn.textContent;
  btn.disabled = true; btn.textContent = "测试中…";
  try {
    const r = await api("/admin/speedtest", { tests: [{ id: p.id, model }] });
    const one = (r.results || [])[0] || { ok: false, error: "没有返回结果" };
    p.test = one;
    if (one.ok && one.warning) toast(`${p.name} · ${model} 本次测试成功（短请求），耗时 ${fmtLat(one.latency_seconds)}。历史记录：${one.warning}`, "warn");
    else if (one.rate_limited) toast(`${p.name} · ${model} 限流：${one.error || ""}`, "warn");
    else if (one.ok) toast(`${p.name} · ${model} 短请求通过，延迟 ${fmtLat(one.latency_seconds)}；未验证长上下文和流式调用`, "ok");
    else toast(`${p.name} · ${model} 不可用：${one.error || ""}`, "bad");
  } catch (e) {
    toast("测试失败：" + e.message, "bad");
  }
  btn.disabled = false; btn.textContent = old;
  renderModal();
}
// 只更新本地预览：把输入框的值同步到 dataset.currentGroup 和 has-group 样式
// 不触发 API 调用 —— 真正的提交由 commitGroupChanges 负责
function updateLocalGroupPreview(gb) {
  if (!gb) return;
  const raw = (gb.value || "").trim();
  const real = gb.dataset.groupFor;
  // 视觉上的"默认值"就是模型名本身，所以 input 空/等于模型名时显示模型名
  const display = raw || real;
  gb.dataset.currentGroup = display;
  gb.classList.toggle("has-group", !!raw && raw !== real);
  // 同步面板下方的分组汇总
  const card = gb.closest(".up");
  if (card) updatePanelGroupsSummary(card.dataset.id);
}
// 读取卡片上所有分组输入，批量调用 set-group 写入服务端，成功后刷新
async function commitGroupChanges(id) {
  const card = document.querySelector(`#upstreams .up[data-id="${cssEsc(id)}"]`);
  if (!card) return;
  const inputs = [...card.querySelectorAll("input[data-group-for]")];
  if (!inputs.length) { toast("没有可确认的分组", "bad"); return; }
  const mapping = {};
  inputs.forEach(gb => {
    const real = gb.dataset.groupFor;
    const g = (gb.value || "").trim();
    if (g && g !== real) mapping[real] = g;
  });
  if (!Object.keys(mapping).length) {
    toast("所有分组都已是默认值，无需确认", "ok");
    return;
  }
  try {
    await api("/admin/set-group", { id, mapping });
    toast(`已确认 ${Object.keys(mapping).length} 个分组映射`, "ok");
    await refresh();
  } catch (e) {
    toast("确认分组失败：" + e.message, "bad");
  }
}
// 给某个上游的真实模型指定分组名（留空 = 用真实名本身）
async function saveGroup(id, realModel, group) {
  // 分组名和模型名一样 = 没改过，等价于「不分组」，存空串
  let g = (group || "").trim();
  if (g === realModel) g = "";
  // 先把界面同步成新值。
  // 注意：不能指望 refresh() 来更新 —— 输入框此刻还是聚焦状态，
  // renderUpstreams() 会为了保护正在输入的内容而跳过重建（见那里的守卫），
  // 所以这里要自己把值和状态一起改掉。
  syncGroupInput(id, realModel, g);

  try {
    await api("/admin/set-group", { id, mapping: { [realModel]: g } });
    toast(g ? `「${realModel}」已归到分组「${g}」` : `「${realModel}」已恢复用原名`, "ok");
    await refresh();
  } catch (e) {
    toast("设置分组失败：" + e.message, "bad");
    await refresh();
  }
}

// 把某个分组输入框的值、当前分组标记、以及面板下方的汇总一起更新
function syncGroupInput(upstreamId, realModel, group) {
  const inputs = [...document.querySelectorAll("#upstreams input[data-group-for]")];
  const gb = inputs.find(i => i.dataset.groupFor === realModel
                              && i.dataset.upstream === upstreamId);
  if (!gb) return;
  // 默认显示模型名(不填 = 用真实名当分组名)
  const shown = group || realModel;
  gb.value = shown;
  gb.dataset.currentGroup = shown;
  gb.classList.toggle("has-group", !!group && group !== realModel);
  updatePanelGroupsSummary(upstreamId);
}

  // 更新卡片下方那行「本上游的分组：xxx ← 模型1, 模型2」
function updatePanelGroupsSummary(upstreamId) {
  const card = document.querySelector(`#upstreams .up[data-id="${cssEsc(upstreamId)}"]`);
  if (!card) return;
  const box = card.querySelector(".panel-groups");
  if (!box) return;

  const gmap = {};
  card.querySelectorAll("input[data-group-for]").forEach(i => {
    const real = i.dataset.groupFor;
    const g = (i.value || "").trim();
    (gmap[g] = gmap[g] || []).push(real);
  });
  // 空分组名表示「用真实名」，不算别名，不用展示
  const aliased = Object.entries(gmap).filter(([g, ms]) => g && (ms.length > 1 || g !== ms[0]));
  const total = card.querySelectorAll("input[data-group-for]").length;
  box.innerHTML = `已选 ${total} 个。` + (aliased.length
    ? `本上游的分组：` + aliased.map(([g, ms]) =>
        `<span class="badge purple mono">${esc(g)}</span> ← ${ms.map(esc).join(", ")}`).join("　")
    : "");
}

// 选择器里转义特殊字符（模型名里可能有冒号、斜杠、点号等）
function cssEsc(s) {
  return String(s).replace(/["\\]/g, "\\$&");
}

async function selectModels(id, body, opts = {}) {
  try {
    const r = await api("/admin/select-models", { id, ...body });
    // 勾选是高频操作，成功时不弹提示，免得刷屏
    const added = r.added || [], removed = r.removed || [];
    if (added.length && removed.length) {
      toast(`已更新：新增 ${added.length} 个、去掉 ${removed.length} 个模型`,
            "ok");
    } else if (added.length) {
      toast(`已选用 ${added.length} 个模型`, "ok");
    } else if (removed.length) {
      toast(`已取消 ${removed.length} 个模型`, "");
    }
    await refresh();
    if (!opts.keepPanel) { /* 保持面板展开状态由 expanded 决定 */ }
  } catch (e) { toast("保存失败：" + e.message, "bad"); await refresh(); }
}

async function fetchModels(ids) {
  const label = ids ? "该上游" : "全部上游";
  toast(`正在拉取${label}的模型列表…`);
  try {
    const r = await api("/admin/fetch-models", ids ? { ids } : {});
    const res = r.results || [];
    const ok = res.filter(x => x.ok);
    const bad = res.filter(x => !x.ok);
    if (ok.length) {
      toast("拉取完成：" + ok.map(x => `${x.name} ${x.count} 个`).join("，"), "ok");
    }
    bad.forEach(x => toast(`${x.name} 拉取失败：${x.error}`, "bad"));
    // 拉完直接把勾选面板展开，让用户马上挑要用的模型（默认一个都不选）
    ok.forEach(x => { expanded[x.id] = true; });
    if (ok.length) {
      const total = ok.reduce((a, x) => a + x.count, 0);
      if (total) toast("勾选你要用的模型 —— 已选中的才会参与路由", "") ;
      setTab("up");   // 紧接着就是要挑模型，直接把「上游来源」翻出来
    }
    await refresh();
  } catch (e) { toast("拉取失败：" + e.message, "bad"); }
}

async function autoConfigure() {
  try {
    const r = await api("/admin/auto-configure", {});
    const n = (r.created || []).length, s = (r.skipped || []).length;
    toast(n ? `已自动配置 ${n} 个模型${s ? `，跳过已配置的 ${s} 个` : ""}`
            : (s ? `没有新模型需要配置（${s} 个已配置过）` : "还没有拉到任何模型"), n ? "ok" : "");
    await refresh();
  } catch (e) { toast("操作失败：" + e.message, "bad"); }
}

async function addUpstream() {
  const name = $("#newName").value.trim();
  const base_url = $("#newUrl").value.trim();
  const api_key = $("#newKey").value;
  if (!base_url) { toast("请填上游地址", "bad"); $("#newUrl").focus(); return; }
  const btn = $("#addBtn");
  btn.disabled = true;
  try {
    const r = await api("/admin/upstream/add", { name, base_url, api_key });
    $("#newName").value = ""; $("#newUrl").value = ""; $("#newKey").value = "";
    toast("已添加，正在拉取它的模型…", "ok");
    await refresh();
    // 直接用接口返回的 id，别靠 base_url 回查（state 可能还没来得及刷新）。
    // 模型列表拉取放到后台，不要让「添加」按钮一直等一个慢上游。
    const newId = r.upstream && r.upstream.id;
    if (newId) fetchModels([newId]).catch(() => {});
  } catch (e) {
    toast("添加失败：" + e.message, "bad");
  }
  btn.disabled = false;
}

async function updateUpstream(patch, msg) {
  try {
    await api("/admin/upstream/update", patch);
    if (msg) toast(msg + "成功", "ok");
    await refresh();
  } catch (e) { toast("保存失败：" + e.message, "bad"); await refresh(); }
}

let editId = null;

// 编辑弹窗里的 Key 列表。每个条目：
//   { id: 原有 Key 的下标（新增行为 null）, name: 名字, hint: 打码值, typed: 这次输入的新 Key,
//     enabled: 启用状态（禁用 = 保留但不参与轮转）, model: 逐 Key 测试用哪个模型,
//     test: 最近一次测试结果 }
// 数组顺序 = 列表顺序（上移/下移直接换位置），保存时整表提交。
let editKeys = [];
// 这个上游勾选了的模型 —— 逐 Key 测试的模型候选（每行下拉里选）
let editModels = [];

// 把界面上输入的名字/Key 值收回到 editKeys（重排、删除前必须先收回，否则输入会丢）
function syncEditKeys() {
  const rows = $("#eKeyList").children;
  editKeys.forEach((k, i) => {
    const row = rows[i];
    if (!row) return;
    k.name = row.querySelector(".ekey-name").value.trim();
    k.typed = row.querySelector(".ekey-input").value;
    k.enabled = row.querySelector(".ekey-en").checked;
    if (editModels.length) k.model = row.querySelector(".ekey-model").value;
  });
}

// 逐 Key 测试结果画到行上：✓ 延迟 / 限流 / ✗ 状态码
function paintKeyResult(el, t) {
  if (!t) { el.textContent = ""; el.title = ""; return; }
  el.title = (t.model ? `模型 ${t.model}；` : "") + (t.error || "") +
             (t.key_hint ? `（Key ${t.key_hint}）` : "");
  if (t.ok) {
    el.textContent = "✓ " + fmtLat(t.latency_seconds);
    el.style.color = "var(--ok)";
  } else if (t.rate_limited) {
    el.textContent = "限流";
    el.style.color = "var(--warn)";
  } else {
    el.textContent = "✗ " + (t.status || "");
    el.style.color = "var(--bad)";
  }
}

function renderKeyList() {
  const box = $("#eKeyList");
  box.innerHTML = "";
  editKeys.forEach((k, i) => {
    const row = document.createElement("div");
    row.className = "row";
    row.style.cssText = "gap:6px;align-items:center";
    row.innerHTML = `
      <input class="ekey-en" type="checkbox" title="启用这个 Key；取消 = 禁用（保留但不参与轮转和故障切换）">
      <button class="mini ekey-mv" type="button" title="上移" ${i === 0 ? "disabled" : ""}>↑</button>
      <button class="mini ekey-mv" type="button" title="下移" ${i === editKeys.length - 1 ? "disabled" : ""}>↓</button>
      <input class="ekey-name" placeholder="名字" title="给这个 Key 起个名字（可选）" style="width:88px">
      <input class="mono ekey-input" type="password" autocomplete="off"
        placeholder="${k.id == null ? "sk-…" : "留空表示不改"}" style="flex:1">
      <span class="muted" style="font-size:12px;min-width:80px">${k.id == null ? "(新)" : esc(k.hint)}</span>
      <span class="ekey-res" style="font-size:12px"></span>
      <select class="ekey-model" title="选一个模型来测这个 Key（来自上游勾选的模型）"
        style="max-width:170px;font-size:12px"></select>
      <button class="mini ekey-test" type="button">测</button>
      <button class="mini danger ekey-del" type="button" title="删除此 Key">x</button>`;
    const nameIn = row.querySelector(".ekey-name");
    const keyIn = row.querySelector(".ekey-input");
    const modelSel = row.querySelector(".ekey-model");
    const enChk = row.querySelector(".ekey-en");
    const testBtn = row.querySelector(".ekey-test");
    const resEl = row.querySelector(".ekey-res");
    nameIn.value = k.name || "";
    keyIn.value = k.typed || "";
    enChk.checked = k.enabled !== false;
    row.style.opacity = enChk.checked ? "" : "0.55";
    enChk.addEventListener("change", () => {
      row.style.opacity = enChk.checked ? "" : "0.55";
    });
    paintKeyResult(resEl, k.test);
    if (editModels.length) {
      editModels.forEach(m => {
        const opt = document.createElement("option");
        opt.value = m;
        opt.textContent = m;
        modelSel.appendChild(opt);
      });
      modelSel.value = (k.model && editModels.includes(k.model)) ? k.model : editModels[0];
    } else {
      const opt = document.createElement("option");
      opt.textContent = "没勾选模型";
      modelSel.appendChild(opt);
      modelSel.disabled = true;
    }
    const mv = row.querySelectorAll(".ekey-mv");
    mv[0].onclick = () => {
      syncEditKeys();
      if (i > 0) { [editKeys[i - 1], editKeys[i]] = [editKeys[i], editKeys[i - 1]]; renderKeyList(); }
    };
    mv[1].onclick = () => {
      syncEditKeys();
      if (i < editKeys.length - 1) { [editKeys[i + 1], editKeys[i]] = [editKeys[i], editKeys[i + 1]]; renderKeyList(); }
    };
    row.querySelector(".ekey-del").onclick = () => {
      syncEditKeys();
      editKeys.splice(i, 1);
      renderKeyList();
    };
    // 能测的条件：上游勾选过模型（下拉有候选）、而且这行有 Key（已保存的，或刚输入的新值）
    const canTest = () => editModels.length > 0 && (k.id != null || !!keyIn.value.trim());
    testBtn.disabled = !canTest();
    testBtn.title = editModels.length
      ? "用这行的 Key 和选中的模型发一个最小请求"
      : "先在上游勾选模型，再测 Key";
    keyIn.addEventListener("input", () => { testBtn.disabled = !canTest(); });
    testBtn.onclick = async () => {
      syncEditKeys();
      const kk = editKeys[i];
      testBtn.disabled = true;
      testBtn.textContent = "…";
      resEl.style.color = "var(--dim)";
      resEl.textContent = "测试中";
      try {
        const r = await api("/admin/speedtest", { key_test: {
          id: editId, key_index: kk.id, key_value: (kk.typed || "").trim(),
          model: kk.model,
        }});
        kk.test = r.result || { ok: false, error: "没有结果" };
      } catch (e) {
        kk.test = { ok: false, error: e.message };
      }
      renderKeyList();  // 结果记在条目上，重绘后还在
    };
    box.appendChild(row);
  });
  $("#eKeyHint").textContent = editKeys.length
    ? `(${editKeys.length} \u4e2a Key)` : "(\u6ca1\u6709 Key)";
  $("#eKeyStrategyWrap").style.display = editKeys.length > 1 ? "" : "none";
}

function editUpstream(id) {
  const u = (state.upstreams || []).find(x => x.id === id);
  if (!u) return;
  editId = id;
  $("#eName").value = u.name;
  $("#eUrl").value = u.base_url;
  // 多线路：把现有镜像铺到文本框（含主地址），改完保存时整体回传
  const mirs = (u.base_urls && u.base_urls.length > 1)
    ? u.base_urls
    : [u.base_url].filter(Boolean);
  $("#eMirrors").value = mirs.join("\n");
  $("#eUA").value = u.user_agent || "";
  // 出口：跟随全局就是 inherit；单独指定过的会回显自己的模式与代理地址
  $("#eNetMode").value = u.net_mode || "inherit";
  $("#eProxyUrl").value = u.proxy_url || "";
  $("#eNetResult").textContent = "";
  const hints = u.key_hints || (u.has_key ? [u.key_hint] : []);
  const names = u.key_names || [];
  const dis = u.key_disabled || [];
  editModels = [...(u.selected_models || [])];
  editKeys = hints.map((h, i) => ({ id: i, name: names[i] || "", hint: h, typed: "",
                                    enabled: !dis[i],
                                    model: editModels[0] || "" }));
  renderKeyList();
  $("#eKeyStrategy").value = u.key_strategy || "failover";
  $("#eEnabled").checked = u.enabled;
  $("#eTimeout").value = u.first_timeout_seconds || "";
  // 接口类型：默认 chat（老上游没有这个字段）
  $("#eApiMode").value = u.api_mode || "chat";
  // 仅流式模型：有就按逗号铺开显示，没有就是空（留空 = 全按原样发）
  $("#eStreamOnly").value = (u.stream_only_models || []).join(", ");
  $("#eStatus").textContent = "";
  $("#editModal").hidden = false;
  $("#eName").focus();
}

$("#eAddKey").onclick = () => {
  syncEditKeys();
  editKeys.push({ id: null, name: "", hint: "", typed: "", enabled: true });
  renderKeyList();
  const inputs = $("#eKeyList").querySelectorAll(".ekey-input");
  if (inputs.length) inputs[inputs.length - 1].focus();
};

// 编辑弹窗里「测一下这家的几条路」：对这个上游的主机，把每条路各握手一次。
// 弹窗里刚输入的代理地址也一起测（不用先保存）。
$("#eNetBench").onclick = async () => {
  const out = $("#eNetResult");
  const btn = $("#eNetBench");
  btn.disabled = true;
  out.textContent = "测路中…";
  try {
    const r = await api("/admin/net/bench", {
      ids: [editId],
      proxy_url: ($("#eProxyUrl").value || "").trim(),
    });
    const row = (r.results || [])[0];
    if (!row || !(row.routes || []).length) {
      out.textContent = "没有可测的路径";
    } else {
      out.innerHTML = (row.routes || []).map(x => {
        const text = netModeText(x.mode) + " " +
          (x.ok ? Math.round(x.ms) + "ms" : "失败");
        const best = row.fastest && x.mode === row.fastest;
        const col = x.ok ? (best ? "var(--ok)" : "var(--dim)") : "var(--bad)";
        const title = x.ok
          ? `${x.label}｜典型 ${Math.round(x.ms)}ms，最好 ${Math.round(x.min_ms)}ms，最慢 ${Math.round(x.max_ms)}ms`
          : (x.error || x.label);
        return `<span style="margin-right:9px;color:${col}" title="${esc(title)}">` +
          `${best ? "★ " : ""}${esc(text)}</span>`;
      }).join("");
      if (row.fastest) {
        const use = document.createElement("button");
        use.className = "mini";
        use.type = "button";
        use.textContent = `用最快的（${netModeText(row.fastest)}）`;
        use.onclick = () => {
          if (row.fastest === "custom") { toast("你自己的代理最快，模式保持当前即可", "ok"); return; }
          $("#eNetMode").value = row.fastest;
          toast(`已选「${netModeText(row.fastest)}」，记得点保存`, "ok");
        };
        out.appendChild(use);
      }
    }
  } catch (e) {
    out.textContent = "测路失败：" + e.message;
  }
  btn.disabled = false;
};

async function saveUpstreamEdit() {
  if (!editId) return;
  const capNum = el => {
    const raw = (el.value || "").trim();
    if (raw === "") return 0;
    const n = Math.floor(Number(raw));
    return Number.isFinite(n) && n > 0 ? n : 0;
  };
  // 收集 Key 列表：整表按当前顺序提交（名字 + 下标 + 启用状态 + 这次输入的 Key 值；
  // 老 Key 留空 = 后端保留原值，新增行留空 = 不添加）
  syncEditKeys();
  const keys = editKeys
    .map(k => ({ id: k.id, name: k.name, key: (k.typed || "").trim(),
                 enabled: k.enabled !== false }))
    .filter(k => k.key || k.id != null);
  const patch = {
    id: editId,
    name: $("#eName").value.trim(),
    base_url: $("#eUrl").value.trim(),
    enabled: $("#eEnabled").checked,
    first_timeout_seconds: capNum($("#eTimeout")),
    user_agent: $("#eUA").value.trim(),
    // 接口类型：chat = /v1/chat/completions，responses = /v1/responses
    api_mode: $("#eApiMode").value || "chat",
    api_keys: keys,
    key_strategy: $("#eKeyStrategy").value || "failover",
    // 出口：直连 / 规则 / 强制代理 / 跟随全局；proxy_url 留空 = 用全局端口
    net_mode: $("#eNetMode").value || "inherit",
    proxy_url: ($("#eProxyUrl").value || "").trim(),
    // 仅流式模型：逗号/空格/换行分隔都认，去重后提交（留空 = 清空，全部按原样发）
    stream_only_models: String($("#eStreamOnly").value || "")
      .split(/[\s,;]+/).map(s => s.trim()).filter(Boolean)
      .filter((v, i, a) => a.indexOf(v) === i),
  };
  if (!patch.base_url) { toast("\u5730\u5740\u4e0d\u80fd\u4e3a\u7a7a", "bad"); return; }

  // 多线路：文本域一行一个；主地址一定在里面（不在就补到最前面）
  const mirrorLines = String($("#eMirrors").value || "")
    .split(/\r?\n/).map(s => s.trim()).filter(Boolean)
    .filter((v, i, a) => a.indexOf(v) === i);
  if (!mirrorLines.includes(patch.base_url)) mirrorLines.unshift(patch.base_url);
  patch.base_urls = mirrorLines;

  const btn = $("#eSave");
  btn.disabled = true;
  $("#eStatus").textContent = "\u4fdd\u5b58\u4e2d\u2026";
  try {
    await api("/admin/upstream/update", patch);
    $("#editModal").hidden = true;
    toast("\u5df2\u4fdd\u5b58", "ok");
    await refresh();
  } catch (e) {
    toast("\u4fdd\u5b58\u5931\u8d25\uff1a" + e.message, "bad");
    $("#eStatus").textContent = "";
  }
  btn.disabled = false;
}

async function delUpstream(id) {
  const u = (state.upstreams || []).find(x => x.id === id);
  if (!u) return;
  if (!confirm(`删除上游「${u.name}」？\n\n引用它的模型路由也会一并移除。`)) return;
  try {
    const r = await api("/admin/upstream/delete", { id });
    const aff = (r.affected_models || []);
    toast(`已删除「${r.deleted}」${aff.length ? `，同时影响：${aff.join("、")}` : ""}`, "ok");
    await refresh();
  } catch (e) { toast("删除失败：" + e.message, "bad"); }
}

// 查这个上游的余额（只有 New API / One API 那套接口能查）。
async function checkBalance(id) {
  toast("正在查余额…");
  try {
    const r = await api("/admin/balance", { ids: [id] });
    const one = (r.results || [])[0];
    if (!one) { toast("没查到结果", "bad"); }
    else if (one.ok) {
      toast(`${one.name}：余额 ${moneyText(one.remaining)}（总 ${moneyText(one.total)} · 已用 ${moneyText(one.used)}）`, "ok");
    } else {
      toast(`${one.name}：查不到余额（${one.error || "上游不支持"}）`, "warn");
    }
  } catch (e) { toast("查余额失败：" + e.message, "bad"); }
  await refresh();
}

async function testUpstream(id) {
  const u = (state.upstreams || []).find(x => x.id === id);
  const n = u ? ((u.selected_models || []).length || 1) : 1;
  toast(`正在测速(${n} 个模型各测一次)…`);
  try {
    const r = await api("/admin/speedtest", { ids: [id] });
    const res = r.results || [];
    const sum = (r.upstreams || [])[0];
    // 每个模型一条结果，都存下来，卡片上逐条展示
    res.forEach(x => { if (x && x.id) lastTestByModel[`${x.id}|${x.model}`] = x; });
    if (sum) lastTest[id] = sum;
    if (!res.length) { toast("这个上游还没勾选任何模型，先拉取并勾选", "bad"); }
    else if (res.some(x => x.warning)) {
      const nOk = res.filter(x => x.ok).length;
      toast(`${sum ? sum.name : ""}：本次短请求测试 ${nOk}/${res.length} 个成功。历史记录：` + res.filter(x => x.warning).map(x => `${x.model}：${x.warning}`).join("；"), "warn");
    } else if (sum && sum.ok) {
      const nOk = res.filter(x => x.ok).length;
      toast(`${sum.name}：${nOk}/${res.length} 个模型短请求通过，最快 ${fmtLat(sum.latency_seconds)}`, "ok");
    } else if (sum && sum.rate_limited) {
      toast(`${sum.name} 限流(暂时不可用)`, "warn");
    } else {
      toast(`${sum ? sum.name : ""} 全部模型都失败`, "bad");
    }
  } catch (e) { toast("测速失败：" + e.message, "bad"); }
  await refresh();
}

async function saveModel() {
  if (!editing) return;
  collectQuotas();
  const entries = editing.picks.map(p => ({
    upstream: p.id, model: p.model || editing.name, enabled: p.enabled,
    // 没填就不传 quota，后端当「不限量」
    ...(p.quota === "" || p.quota == null ? {} : { quota: p.quota }),
  }));
  // 没勾选参与的上游照样存（enabled:false，调用时被跳过），
  // 这样下次重新勾上就能恢复，用户不会误以为"删掉了要手动加回"
  const active = entries.filter(e => e.enabled);
  if (!active.length) {
    toast("至少要留一个参与的上游（或者删掉这个模型的路由）", "bad");
    return;
  }
  const live = active.filter(e => e.quota !== 0);
  if (!live.length) {
    toast("所有参与的上游额度都是 0，这个分组就没人能调了。至少留一个不限量或大于 0 的。", "bad");
    return;
  }
  const name = editing.name;
  $("#mSave").disabled = true; $("#mStatus").textContent = "保存中…";
  try {
    await api("/admin/model/save", {
      name: editing.name, strategy: editing.strategy, affinity: editing.affinity || "auto", entries,
    });
    toast(`已保存「${name}」的路由`, "ok");
    await refresh();
    openModel(name);
  } catch (e) { toast("保存失败：" + e.message, "bad"); }
  $("#mSave").disabled = false; $("#mStatus").textContent = "";
}

/* ================= 具名排序：保存 / 切换 / 删除 / 按测速重排 ================= */
function currentOrder() {
  // 用「当前弹窗里的顺序」作为要保存的顺序，包含额度为 0 的（它们也在路由里）
  return (editing ? editing.picks : []).map(p => ({
    upstream: p.id, model: p.model || editing.name,
  }));
}

// 从后端重新拉排序列表同步到弹窗。
// 后端有时会顺手补一个「原始」（第一次保存排序时），本地不重新读就看不到它。
async function refreshOrderings() {
  const r = await api("/admin/model/orderings", { name: editing.name });
  editing.orderings = (r.orderings || []).map(o => o.label);
  editing.activeOrdering = r.active || "";
  return r;
}

async function saveOrdering(label) {
  if (!editing) return;
  const name = (label || "").trim();
  if (!name) { toast("请给这套排序起个名字", "bad"); return; }
  try {
    // 把弹窗里当前的（可能刚拖动过的）顺序一起交上去，
    // 这样「保存当前顺序为…」存的就是眼前看到的这套
    await api("/admin/model/order/save", {
      name: editing.name, label: name, order: currentOrder(),
    });
    await refreshOrderings();
    if (!(editing.orderings || []).includes(name)) {
      editing.orderings = [...(editing.orderings || []), name];
    }
    editing.sortLabel = name;
    $("#mSortNew").hidden = true;
    $("#mSortName").value = "";
    toast(`已把当前顺序存成「${name}」`, "ok");
    renderSortBox();
  } catch (e) { toast("保存排序失败：" + e.message, "bad"); }
}

// 把弹窗里改过但还没保存的额度写进服务器（只动额度，不动顺序）
async function flushQuotas() {
  if (!editing) return;
  collectQuotas();
  const entries = editing.picks
    .filter(p => p.quota !== "" && p.quota != null)
    .map(p => ({ upstream: p.id, model: p.model || editing.name, quota: p.quota }));
  if (!entries.length) return;
  try {
    await api("/admin/model/quota", { name: editing.name, entries });
  } catch (e) {
    // 还没建路由（没有 entries）时会 404，这种时候本来也没额度可存
  }
}

async function applyOrdering(label) {
  if (!editing) return;
  label = (label || "").trim();
  if (!label) return;
  // 切换顺序前先把额度变更落盘，否则会被这次切换覆盖掉
  await flushQuotas();
  try {
    const r = await api("/admin/model/order/apply", { name: editing.name, label });
    // 顺序变了 → 按服务器返回的顺序重排弹窗里的 picks，额度仍按成员对应
    const order = r.entries || [];
    editing.picks.sort((a, b) => {
      const ia = order.findIndex(e => e.upstream === a.id && e.model === a.model);
      const ib = order.findIndex(e => e.upstream === b.id && e.model === b.model);
      return (ia < 0 ? 999 : ia) - (ib < 0 ? 999 : ib);
    });
    order.forEach(e => {
      const p = editing.picks.find(x => x.id === e.upstream && x.model === e.model);
      if (p) p.quota = e.quota == null ? "" : e.quota;
    });
    editing.activeOrdering = r.active || label;
    editing.sortLabel = label;
    toast(`已切换到排序「${label}」`, "ok");
    renderModal();
  } catch (e) { toast("切换排序失败：" + e.message, "bad"); }
}

async function deleteOrdering(label) {
  if (!editing || !label) return;
  if (!confirm(`删除排序「${label}」？\n\n当前顺序不会变，只是这套名字没了。`)) return;
  try {
    await api("/admin/model/order/delete", { name: editing.name, label });
    await refreshOrderings();
    editing.sortLabel = "";
    toast(`已删除排序「${label}」`, "ok");
    renderSortBox();
  } catch (e) { toast("删除排序失败：" + e.message, "bad"); }
}

async function reorderBySpeed() {
  if (!editing) return;
  const btn = $("#mSortBySpeed");
  btn.disabled = true; btn.textContent = "测速中…";
  try {
    // 先测全部启用的上游，再把结果交给后端重排
    const t = await api("/admin/speedtest", {});
    const results = t.results || [];
    const byId = {};
    results.forEach(x => { if (x && x.id) byId[x.id] = x; });
    // 后端按 upstream 字段认；把 id 也带上，兼容两种字段名
    const payload = results.map(x => ({
      upstream: x.id, id: x.id, ok: x.ok, latency_seconds: x.latency_seconds,
    }));
    const r = await api("/admin/model/reorder-by-speed", {
      name: editing.name, label: "按测速", results: payload,
    });
    results.forEach(x => { lastTest[x.id] = x; });

    // 界面同步成新顺序
    const order = r.order || [];
    editing.picks.sort((a, b) => {
      const ia = order.findIndex(o => o.upstream === a.id && o.model === a.model);
      const ib = order.findIndex(o => o.upstream === b.id && o.model === b.model);
      return (ia < 0 ? 999 : ia) - (ib < 0 ? 999 : ib);
    });
    // 重新拉一遍排序列表：后端顺手建了「原始」，得让它出现在下拉里，
    // 否则用户想切回去都找不到入口
    await refreshOrderings();
    editing.sortLabel = (editing.orderings || []).includes(r.label || "按测速")
      ? (r.label || "按测速") : (editing.orderings || [])[0] || "";

    const ok = results.filter(x => x.ok).length;
    const failed = results.filter(x => !x.ok).length;
    toast(`已按测速重排并存成「${r.label}」：${ok} 个正常` +
          (failed ? `，${failed} 个失败（排到最后）` : ""), "ok");
    renderModal();
  } catch (e) { toast("按测速重排失败：" + e.message, "bad"); }
  btn.disabled = false; btn.textContent = "⚡ 按测速重排并保存";
}

async function deleteModelByName(name) {
  if (!name) return false;
  if (!confirm(`删除「${name}」的路由配置？\n\n删除后这个模型会退回「自动」模式(按已拉取的模型列表挑上游)；\n以后勾选别的模型也不会自动把它建回来，需要时在弹窗里保存即可恢复。`)) return false;
  try {
    await api("/admin/model/delete", { name });
    toast(`已删除「${name}」的路由配置`, "ok");
    await refresh();
    return true;
  } catch (e) { toast("删除失败：" + e.message, "bad"); return false; }
}

async function deleteModel() {
  if (!editing) return;
  const name = editing.name;
  if (await deleteModelByName(name)) {
    editing = null;
    selectedModel = null;
    await refresh();
    if (!editing) {
      const d = $("#modelDetail"), e = $("#modelEmpty");
      if (d) d.hidden = true;
      if (e) e.hidden = false;
    }
  }
}

/* ================= 请求记录 tab：统计 + 请求列表 ================= */
const openReqs = new Set();     // 展开了详情的请求（key 用 at）
let lastSideHash = "";          // 内容没变就不重建，省得把展开状态冲掉
// 列表一次取这么多条，点「显示更多」向后追加。
// 翻页是对**落盘流水**做的，不是内存上限 —— 只要流水里有就能一直往下翻。
const REQ_PAGE = 100;
let reqPage = 1;
// 当前这一页流水数据（含 total），以及各维度的筛选项
let journalPage = null;
let journalIndex = null;
// 展开详情时按需取回的完整记录（列表本身不带正文，省流量）
// 展开某条时按需取回的完整记录缓存（列表不带正文，省流量），key = at
const reqTextCache = new Map();

// 当前筛选的可读描述，用在角标上
// 上游筛选值可能是「上游」或「上游 · 模型」组合（同一分组里一个上游挂了多个模型）。
// 值用 JSON [上游, 模型] 编码，避免名字里带分隔符时拆错；老的纯上游值也兼容。
function parseUpFilter(v) {
  if (!v) return { upstream: "", model: "" };
  try {
    const a = JSON.parse(v);
    if (Array.isArray(a)) return { upstream: String(a[0] || ""), model: String(a[1] || "") };
  } catch (e) {}
  return { upstream: String(v), model: "" };
}
function upFilterValue(upstream, model) {
  return JSON.stringify([upstream || "", model || ""]);
}
function upFilterLabel(v) {
  const p = parseUpFilter(v);
  return p.model && p.model !== "-" ? `${p.upstream} · ${p.model}` : p.upstream;
}

function curFilterLabel() {
  const day = ($("#sideDay") && $("#sideDay").value) || "";
  const month = ($("#sideMonth") && $("#sideMonth").value) || "";
  const g = ($("#sideFilter") && $("#sideFilter").value) || "";
  const u = ($("#sideUpstream") && $("#sideUpstream").value) || "";
  const parts = [];
  if (day) parts.push(day);
  else if (month) parts.push(month + "（月）");
  if (g) parts.push(g);
  if (u) parts.push(upFilterLabel(u));
  return parts.length ? "[" + parts.join(" · ") + "]" : "[全部]";
}

// 当前筛选条件（传给后端）
function curJournalFilter() {
  let day = ($("#sideDay") && $("#sideDay").value) || "";
  const month = ($("#sideMonth") && $("#sideMonth").value) || "";
  // 「今天」这个 option 要等 index 回来才填得进下拉框；在它出现之前如果 day 为空，
  // 第一次请求就会拉全量、之后才切成当天（刷新时闪一下全部记录就是这个）。
  // 首次进入直接按今天查，不再依赖下拉框有没有那个 option。
  if (!day && !month && !journalDayDefaulted) day = dayKeyOf();
  const g = ($("#sideFilter") && $("#sideFilter").value) || "";
  const u = ($("#sideUpstream") && $("#sideUpstream").value) || "";
  const up = parseUpFilter(u);
  // 选了具体某天时，月份条件就没必要再传了（天更具体）
  return { day, month: day ? "" : month, group: g,
           upstream: up.upstream, model: up.model };
}

async function loadJournal() {
  const f = curJournalFilter();
  const source = ($("#sideSource") && $("#sideSource").value) || "all";
  const qs = new URLSearchParams({
    limit: String(REQ_PAGE),
    offset: String((reqPage - 1) * REQ_PAGE),
  });
  if (f.day) qs.set("day", f.day);
  if (f.month) qs.set("month", f.month);
  if (f.group) qs.set("group", f.group);
  if (f.upstream) qs.set("upstream", f.upstream);
  if (f.model) qs.set("model", f.model);
  try {
    if (source === "router") {
      journalPage = await api("/admin/journal?" + qs.toString(), undefined, "GET");
    } else {
      if (!f.day && !f.month) qs.set("range", "all");
      qs.set("metric", timingMode());
      journalPage = await api("/sessions/records?" + qs.toString(), undefined, "GET");
    }
  } catch (e) {
    journalPage = { entries: [], total: 0, error: e.message, scope: source === "router" ? "router" : "all" };
  }
  // 上游下拉跟着当前分组走：只有选过分组才列上游，避免全量时列一长串。
  fillUpstreamOptions();
  fillFilterOptions();
  renderTabNums();
  return journalPage;
}

// 按当前分组列上游（带失败数）。数据来自 query_journal 的 upstreams 字段，
// 它在「天/月/分组」筛选之后统计，所以这里天然只有该分组用过的上游。
function fillUpstreamOptions() {
  const sel = $("#sideUpstream");
  if (!sel) return;
  const g = ($("#sideFilter") && $("#sideFilter").value) || "";
  const items = (journalPage && journalPage.upstreams) || [];
  const usable = g ? items : [];
  // 选项按「上游 · 真实模型」细分：同一分组里一个上游挂了多个模型时，
  // 能分别看各自的请求情况（老记录没有模型名时退回纯上游）。
  const key = usable.map(x => upFilterValue(x.upstream, x.model) + ":" + x.failed).join("|");
  if (sel.dataset.opts === key) return;
  const cur = sel.value;
  const opts = usable.map(u =>
    `<option value='${esc(upFilterValue(u.upstream, u.model))}'>${esc(u.key)}${u.failed ? `（${u.failed} 败）` : ""}</option>`
  );
  sel.innerHTML = `<option value="">全部上游</option>` + opts.join("");
  sel.value = usable.some(u => upFilterValue(u.upstream, u.model) === cur) ? cur : "";
  sel.dataset.opts = key;
  sel.disabled = !g;
}

async function loadJournalIndex() {
  try {
    journalIndex = await api("/admin/journal/index", undefined, "GET");
  } catch (e) {
    journalIndex = null;
  }
  return journalIndex;
}

// 把索引里的天 / 月 / 分组填进筛选下拉。
// 只列真有过记录的那些值，不给空选项 —— 选一个没数据的日期只会
// 得到一片空白，那很难分辨是「那会儿没调用」还是「筛坏了」。
// 例外：日期下拉会额外保留「今天」，这样刚过零点、今天还没请求时
// 默认选中今天也能看到明确的「今天还没有请求」。
let journalDayDefaulted = false;   // 首次进入默认选今天；之后尊重用户选择
function fillFilterOptions() {
  if (!journalIndex && !journalPage) return;
  const fill = (sel, items, allLabel, extra = []) => {
    if (!sel) return;
    const base = (items || []).map(x => x.key);
    const opts = base.concat(extra.filter(x => !base.includes(x)));
    const key = opts.join("|");
    if ((sel.dataset.opts || "") === key) return;   // 没变就不重建，免得把选择冲掉
    const cur = sel.value;
    sel.innerHTML = `<option value="">${allLabel}</option>` +
      opts.map(k => `<option value="${esc(k)}">${esc(k)}</option>`).join("");
    sel.value = opts.includes(cur) ? cur : "";
    sel.dataset.opts = key;
  };
  const today = dayKeyOf();
  const idx = journalIndex || {};
  const allScope = journalPage && journalPage.scope === "all";
  const groupItems = allScope
    ? (journalPage.groups || []).map(g => ({ key: g.group }))
    : (idx.groups || []);
  const dayItems = allScope && journalPage.days
    ? journalPage.days
    : (allScope
      ? [...new Set((journalPage.entries || []).map(x => String(x.time || "").slice(0, 10)).filter(Boolean))]
          .map(key => ({ key }))
      : (idx.days || []));
  const monthItems = allScope && journalPage.months
    ? journalPage.months
    : (allScope
      ? [...new Set((journalPage.entries || []).map(x => String(x.time || "").slice(0, 7)).filter(Boolean))]
          .map(key => ({ key }))
      : (idx.months || []));
  fill($("#sideDay"), dayItems, "全部日期", [today]);
  fill($("#sideMonth"), monthItems, "全部月份");
  fill($("#sideFilter"), groupItems, "全部模型 / Provider");
  // 默认按当天看（只默认一次；用户手动切到「全部日期」后不再抢回）
  if (!journalDayDefaulted) {
    journalDayDefaulted = true;
    if ($("#sideDay")) $("#sideDay").value = today;
  }
}

function fmtNum(n) {
  if (n == null) return "—";
  if (n >= 1000000) return (n / 1000000).toFixed(n % 1000000 ? 1 : 0) + "M";
  if (n >= 1000) return (n / 1000).toFixed(n % 1000 ? 1 : 0) + "k";
  return String(n);
}

// 请求次数等计数值显示实际数字，不做 k/M 缩写；token 数继续用 fmtNum。
function fmtCount(n) {
  if (n == null) return "—";
  const v = Number(n);
  return Number.isFinite(v) ? v.toLocaleString("zh-CN") : String(n);
}

// TPS = 输出 token / 秒。优先「生成速度」（总耗时扣掉首字等待，最贴近体感）；
// 旧流水没有 first_chunk_seconds 时退回「全链路速度」（输出 ÷ 总耗时，含首字等待）。
function tpsOf(h) {
  const out = Number((h.usage || {}).output);
  const elapsed = Number(h.elapsed_seconds);
  if (!Number.isFinite(out) || out <= 0 || !Number.isFinite(elapsed) || elapsed <= 0) return null;
  const first = h.first_chunk_seconds != null ? Number(h.first_chunk_seconds) : null;
  let genSecs = first != null && Number.isFinite(first) && elapsed > first ? elapsed - first : null;
  const overall = out / elapsed;
  let gen = null;
  // 上游把整段一次性吐回来时首字≈总耗时，生成窗口趋近 0，
  // 算出来的速度会到几万 tok/s，没有意义：窗口 < 50ms 或速度 > 1000 tok/s 视为不可测。
  if (genSecs != null && genSecs >= 0.05) {
    const g = out / genSecs;
    if (g <= 1000) gen = g; else genSecs = null;
  } else {
    genSecs = null;
  }
  return { out, elapsed, first, genSecs, overall, gen };
}
function fmtTps(v) {
  if (v == null || !Number.isFinite(v)) return "—";
  return v >= 100 ? v.toFixed(0) : v.toFixed(1);
}
function tpsBadge(h) {
  const t = tpsOf(h);
  if (!t) return "";
  const shown = t.gen != null ? t.gen : t.overall;
  const tip = t.gen != null
    ? `生成速度 ${fmtTps(t.gen)} tok/s = 输出 ${fmtNum(t.out)} tok ÷ 生成耗时 ${fmtLat(t.genSecs)}（总耗时扣掉首字等待）；全链路 ${fmtTps(t.overall)} tok/s`
    : `全链路速度 ${fmtTps(t.overall)} tok/s = 输出 ${fmtNum(t.out)} tok ÷ 总耗时 ${fmtLat(t.elapsed)}；首字延迟缺失或生成窗口过短，无法单独算生成速度`;
  return `<span class="badge" title="${tip}">TPS ${fmtTps(shown)} tok/s</span>`;
}
function tpsDetail(h) {
  const t = tpsOf(h);
  if (!t) return `输出速度：<code>—</code>（这条流水没有输出 token 或耗时）`;
  const main = `生成速度：<code>${fmtTps(t.gen)} tok/s</code>（输出 ${fmtNum(t.out)} tok ÷ 生成耗时 ${fmtLat(t.genSecs)}）`;
  const all = `全链路：<code>${fmtTps(t.overall)} tok/s</code>（输出 ${fmtNum(t.out)} tok ÷ 总耗时 ${fmtLat(t.elapsed)}）`;
  return `输出速度：${t.gen != null ? main + "　·　" + all : all + "（首字延迟缺失或生成窗口过短，按全链路口径，含首字等待）"}`;
}

function usageCells(u, withCache) {
  if (!u || !Object.keys(u).length) {
    return `<div class="tkn"><div><b>—</b><span>无用量数据</span></div></div>`;
  }
  const cells = [
    ["input", "输入"],
    ["output", "输出"],
    ["total", "合计"],
  ];
  if (withCache) cells.push(["cache_read", "缓存读"], ["cache_write", "缓存写"]);
  return `<div class="tkn">` + cells.map(([k, label]) => {
    const v = u[k];
    const hl = (k === "cache_read" && v) ? "hl" : "";
    return `<div class="${hl}"><b>${v == null ? "—" : fmtNum(v)}</b><span>${label}</span></div>`;
  }).join("") + `</div>`;
}

function renderSide() {
  // 统计和列表**都以落盘流水为准**（后端启动时会按流水把聚合对平）。
  // 老的实现里列表读的是内存里那 500 条 history，所以白天/昨天的记录翻不到，
  // 而汇总数字来自另一个口径 —— 两边永远对不上。现在统一成一个来源。
  const sr = state.stats_range || {};
  const allScope = journalPage && journalPage.scope === "all";
  const stats = allScope
    ? { totals: journalPage.totals || {}, groups: journalPage.groups || [] }
    : (sr.groups ? sr : (state.call_stats || {}));
  const totals = stats.totals || {};

  const filter = $("#sideFilter").value;
  const upFilter = ($("#sideUpstream") && $("#sideUpstream").value) || "";
  const upSel = parseUpFilter(upFilter);
  const upName = upSel.upstream, upModel = upSel.model;
  const mode = $("#sideMode").value;

  // 这个筛选条件下到底有多少条 —— 用列表自身的总数（同一个来源，必然一致）
  const fullCalls = journalPage ? journalPage.total : (totals.calls != null ? totals.calls : null);
  const shown = journalPage ? (journalPage.entries || []) : [];
  const shownHint = fullCalls != null
    ? `${fmtCount(shown.length)} / ${fmtCount(fullCalls)} 条`
    : `${shown.length} 条`;
  $("#sideCount").textContent = `${curFilterLabel()} ${shownHint}`;
  $("#sideCount").className = "badge" + (totals.failed ? " warn" : "");
  const metricNote = $("#sideMetricNote");
  if (metricNote) {
    const mm = journalPage && journalPage.metric;
    if (!allScope || !mm) {
      metricNote.textContent = "";
      metricNote.title = "";
    } else if (mm.mode !== "magpie") {
      metricNote.textContent = mm.mode === "dsh" ? "DSH 会话计时" : "happycoding 口径";
      metricNote.title = TIMING_TEXT[mm.mode] || "";
    } else if (mm.magpie_available === false) {
      metricNote.textContent = "Magpie 未在运行，已回退";
      metricNote.title = "没读到 Magpie 网关的用量日志，这些轮按 request/header 时刻推算";
    } else {
      const denom = mm.via_magpie != null ? mm.via_magpie : mm.rounds;
      const rest = denom - mm.magpie;
      metricNote.textContent = `网关实测 ${fmtCount(mm.magpie)} / ${fmtCount(denom)} 轮`;
      metricNote.title = "按「会话 + 输入/输出/缓存读 token」和 Magpie 自己的用量日志对账，"
        + "对上的直接用网关实测 ttft_ms / ms（实测 0 ms 误差）。"
        + (rest > 0
          ? `还有 ${fmtCount(rest)} 轮没对上：${fmtCount(mm.header)} 轮按 DSH request/header 时刻推算，其余用 step 口径。`
          : "这个窗口里的每一轮都取到了网关实测值。")
        + (mm.rounds - denom > 0 ? `另有 ${fmtCount(mm.rounds - denom)} 轮没经过 Magpie 网关，用 step 口径。` : "");
    }
  }
  $("#sideCount").title = fullCalls != null
    ? (allScope
      ? `当前筛选下共 ${fullCalls} 条 Agent 会话记录（DSH / Claude / Pi / Codex）；切到「本机路由流水」只看经过 8010 的请求`
      : `当前筛选下共 ${fullCalls} 条调用记录（本机路由落盘流水，可一直往下翻）`)
    : "调用记录";

  // ---- 统计区 ----
  // 选了某个上游时，面板也缩到这个上游：按「它每次被调用」算，
  // 和右侧列表、下面那条对比条同口径。否则客户端的「失败」（请求最终全挂了）
  // 和上游的「败」（它自己失败几次）是两个数，摆一起必然对不上。
  const gSel = (upName && filter)
    ? (stats.groups || []).find(g => g.group === filter) : null;
  const uSelBase = gSel ? (gSel.upstreams || []).find(x => x.upstream === upName) : null;
  // 选了「上游 · 模型」时，统计面板显示这个模型自己的数据
  const uSel = uSelBase && upModel
    ? ((uSelBase.models || []).find(m => m.model === upModel) || null)
    : uSelBase;
  let statHtml;
  if (uSel) {
    const u = uSel.usage || {};
    statHtml = `
  <div class="hint" style="padding:2px 2px 8px">
    当前只统计上游 <b>${esc(upName)}</b>${upModel ? ` 的模型 <code>${esc(upModel)}</code>` : ""}：按它每次被调用算。它失败几次就是几次——
    请求失败后换家在别家成功，这次失败仍算在这个上游头上，所以这里的失败数
    可能大于客户端请求失败数。和右侧列表同口径。
  </div>
  <div class="ov">
    <div class="ovc"><b>${fmtCount(uSel.calls)}</b><span>调用次数</span></div>
    <div class="ovc"><b style="color:var(--ok)">${fmtCount(uSel.ok)}</b><span>成功</span></div>
    <div class="ovc"><b style="color:${uSel.failed ? "var(--bad)" : "var(--dim)"}">${fmtCount(uSel.failed)}</b><span>失败</span></div>
    <div class="ovc"><b>${fmtNum(u.input)}</b><span>输入 token</span></div>
    <div class="ovc"><b>${fmtNum(u.output)}</b><span>输出 token</span></div>
    <div class="ovc"><b style="color:var(--ok)">${fmtNum(u.cache_read)}</b><span>缓存读</span></div>
    <div class="ovc"><b style="color:var(--warn)">${fmtNum(u.cache_write)}</b><span>缓存写</span></div>
  </div>`;
  } else {
  statHtml = `
  <div class="ov">
    <div class="ovc"><b>${fmtCount(totals.calls)}</b><span>总请求</span></div>
    <div class="ovc"><b style="color:var(--ok)">${fmtCount(totals.ok)}</b><span>成功</span></div>
    <div class="ovc"><b style="color:${totals.failed ? "var(--bad)" : "var(--dim)"}">${fmtCount(totals.failed)}</b><span>失败</span></div>
    <div class="ovc"><b>${fmtCount(totals.attempts)}</b><span>上游尝试</span></div>
    <div class="ovc"><b>${fmtNum(totals.input)}</b><span>输入 token</span></div>
    <div class="ovc"><b>${fmtNum(totals.output)}</b><span>输出 token</span></div>
    <div class="ovc"><b style="color:var(--ok)">${fmtNum(totals.cache_read)}</b><span>缓存读</span></div>
    <div class="ovc"><b style="color:var(--warn)">${fmtNum(totals.cache_write)}</b><span>缓存写</span></div>
  </div>
  <div class="hint" style="padding:2px 2px 8px">
    ${allScope
      ? "全部 Agent 会话来自 DSH / Claude / Pi / Codex 的会话记录；会话记录只包含成功轮，失败数不单独提供；首字来自流式首个内容块。切到「本机路由流水」可查看 8010 的失败、重试与上游尝试。"
      : "「总请求」按客户端请求算（失败会换下一个，还算一次）；「上游尝试」按实际发给上游算——换过一次就会多一两次。和右上角每个上游卡片里的「请求/成功/失败」是同一口径。每个上游条上的「N 败」是该上游自己失败的次数（换家过程中失败的也算），所以各条相加会大于客户端「失败」——想看某个上游的真实情况，就在上面的下拉里选它。"}
  </div>`;

  const glist = filter
    ? (stats.groups || []).filter(g => g.group === filter)
    : (stats.groups || []);
  const emptyHint = "所选条件下还没有调用记录。请求一次就会有数据。";
  if (!glist.length) {
    statHtml += `<div class="hint" style="padding:8px 2px">${emptyHint}</div>`;
  } else {
    statHtml += glist.map(g => {
      const maxc = Math.max(...g.upstreams.map(u => u.calls), 1);
      return `
      <div class="gstat">
        <div class="gstat-head">
          <span class="gstat-name">${esc(g.group)}</span>
          <span class="badge blue">${g.upstream_count} 个上游</span>
          ${g.failed ? `<span class="badge bad">${g.failed} 失败</span>` : ""}
          <div class="grow"></div>
          <span class="hint">均 ${fmtLat(g.avg_elapsed)}</span>
        </div>
        <div class="gstat-nums">
          <div><b>${fmtCount(g.calls)}</b><span>请求次数</span></div>
          <div><b style="color:var(--ok)">${fmtCount(g.ok)}</b><span>成功</span></div>
          <div><b style="color:${g.failed ? "var(--bad)" : "var(--dim)"}">${fmtCount(g.failed)}</b><span>失败</span></div>
          ${g.attempts != null ? `<div><b>${fmtCount(g.attempts)}</b><span>上游调用</span></div>` : ""}
          <div><b>${fmtNum(g.usage.input)}</b><span>输入</span></div>
          <div><b>${fmtNum(g.usage.output)}</b><span>输出</span></div>
          <div class="tkn"><b style="color:var(--ok);font-size:13.5px">${fmtNum(g.usage.cache_read)}</b><span>缓存读</span></div>
          <div class="tkn"><b style="color:var(--warn);font-size:13.5px">${fmtNum(g.usage.cache_write)}</b><span>缓存写</span></div>
        </div>
        ${g.upstreams.filter(u => !upName || u.upstream === upName).map(u => {
          // 失败只占条里对应的那一小段，别把整条染红 —— 414 次里错 16 次
          // 看起来不该像全挂了。条总长按调用量，内部再按成功/失败比例分段。
          const bar = (x, sub) => {
            const w = Math.max(3, Math.round(x.calls / maxc * 100));
            const fw = x.calls ? Math.min(w, x.failed / x.calls * w) : 0;
            const okw = w - fw;
            const nm = sub ? (x.model || "-") : x.upstream;
            return `
          <div class="upbar${sub ? " sub" : ""}">
            <span class="nm" title="${esc(nm)}">${esc(nm)}</span>
            <span class="track">${okw > 0 ? `<i class="ok" style="width:${okw}%"></i>` : ""}${
              fw ? `<i class="fail" style="width:${fw}%"></i>` : ""}</span>
            <span class="cnt" title="调用 ${x.calls} 次 · 成功 ${x.ok} · 失败 ${x.failed}">${
              `${fmtCount(x.calls)}<span class="okf">${x.failed ? ` · ${fmtCount(x.failed)}败` : ""}</span>`}</span>
          </div>`;
          };
          // 同一上游在这个分组里挂了多个模型时，上游汇总条下面再列模型细分；
          // 已经选了具体模型就只显示那一行。
          const rows = [];
          if (!upModel) rows.push(bar(u, false));
          const ms = (u.models || []).filter(m => !upModel || m.model === upModel);
          if (ms.length && (upModel || (u.models || []).length > 1)) {
            ms.forEach(m => rows.push(bar(m, true)));
          }
          return rows.join("");
        }).join("")}
      </div>`;
    }).join("");
  }
  }
  $("#cardStats h2").textContent = "统计 · " + curFilterLabel();
  $("#sideStats").innerHTML = statHtml;
  $("#cardStats").hidden = (mode === "req");
  $("#cardReqs").hidden = (mode === "stat");
  // 统计和请求并排；只看其中一块时，那块独占一行
  $("#reqGrid").className = "reqgrid" +
    (mode === "all" ? " two" : (mode === "stat" ? " narrow" : ""));

  // ---- 请求列表 ----
  if (mode === "stat") {
    $("#sideReqs").innerHTML = "";
    return;
  }

  if (!shown.length) {
    $("#sideReqs").innerHTML = `<div class="hint" style="padding:10px 2px">
      ${filter ? "这个分组在所选范围里还没有请求" : "所选条件下还没有请求记录"}</div>`;
    return;
  }

  $("#sideReqs").innerHTML = shown.map((h, i) => {
    const key = String(h.at);
    const open = openReqs.has(key);
    // 这条请求依次试过的上游（每次尝试一步）。老记录可能没有 trail。
    const trail = Array.isArray(h.trail) ? h.trail.filter(Boolean) : [];
    // 按「上游」或「上游 · 模型」筛选时，行代表的应该是该次尝试的结果，
    // 而不是客户端最终结果 —— 否则中途失败的尝试会因为请求后来在别家成功而整行显示绿色。
    const step = upName ? trail.find(s => s.upstream === upName
      && (!upModel || String(s.model || "") === upModel)) : null;
    const rowOk = step ? !!step.ok : h.ok;
    const rowStatus = step ? step.status : h.status;
    const badge = rowOk
      ? `<span class="badge ok">${rowStatus}</span>`
      : `<span class="badge bad">${rowStatus}</span>`;
    // 筛选上游时把「本行主体」显示成被筛的上游（或上游 · 模型），后面再标出最终落在谁身上。
    const subjUp = upName ? (upModel ? `${upName} · ${upModel}` : upName) : h.upstream;
    const fellBack = upName && h.upstream !== upName;
    const use = h.usage || {};
    // 列表接口不带正文（省流量）。展开时如果还没取过，就显示加载中，
    // 由 ensureEntryText() 取回来后再补渲染。
    const full = reqTextCache.get(key);
    const loadingTxt = open && !full;
    return `
    <div class="req ${open ? "open" : ""} ${h.ok && trail.some(s => !s.ok) ? "recovered" : !h.ok ? "failed" : ""}" data-key="${key}">
      <div class="req-head">
        <span class="req-idx mono">${esc(h.time)}</span>
        ${badge}
        <span class="req-model" title="${esc(h.model)}">${esc(h.model)}</span>
        <span class="badge">${esc(subjUp)}</span>
        ${h.key ? `<span class="badge" title="这个请求用的是哪个 Key（名字）">${esc(h.key)}</span>` : ""}
        ${fellBack ? `<span class="hint">→ ${esc(h.upstream)}</span>` : ""}
        <div class="grow"></div>
        ${use.total != null ? `<span class="badge">${fmtNum(use.total)} tok</span>` : ""}
        ${tpsBadge(h)}
        <span class="hint" title="总耗时：从收到请求到整个响应结束（含重试、含流式输出全过程）">总耗时 ${fmtLat(h.elapsed_seconds)}</span>
        ${h.first_chunk_seconds != null ? `<span class="hint" title="首字延迟：从收到请求到第一个内容块到达">首字 ${fmtLat(h.first_chunk_seconds)}</span>` : ""}
        ${h.stream ? `<span class="badge blue">流式</span>` : ""}
        ${requestPathSummary(h, trail)}
      </div>
      ${open ? `
      <div class="req-body">
        <div class="req-sec">
          <h5>Token 用量</h5>
          ${usageCells(use, true)}
          <div class="hint" style="line-height:1.8;margin-top:4px">
            缓存读 = 命中提示缓存的 token；缓存写 = 新建提示缓存的 token（Anthropic / DeepSeek / 中转站字段已统一归一）。
            显示 0 表示上游确实没报缓存，显示「—」表示这条流水是旧版本写的、没有用量字段。
          </div>
        </div>
        <div class="req-sec">
          <h5>耗时</h5>
          <div class="hint" style="line-height:1.8">
            总耗时：<code>${fmtLat(h.elapsed_seconds)}</code>（从收到请求到响应结束，含上游重试与流式输出全过程）<br>
            首字延迟：<code>${h.first_chunk_seconds != null ? fmtLat(h.first_chunk_seconds) : "—"}</code>（首个内容块到达，只有新版本流式流水才有）<br>
            ${h.metric_source ? `计时来源：<code>${{ magpie: "Magpie 网关用量日志", header: "DSH request/header 时刻", step: "DSH step/start" }[h.metric_source] || esc(h.metric_source)}</code><br>` : ""}
            request/header：<code>${h.request_header_seconds != null ? fmtLat(h.request_header_seconds) : "—"}</code>（DSH 写请求头的时刻，相对 step/start；DSH 只在配置变更时才写这条事件，所以大多数轮没有）<br>
            有效内容块：<code>${h.chunks != null ? fmtCount(h.chunks) : "—"}</code>　·　请求方式：<code>${h.stream ? "流式" : "非流式"}</code><br>
             ${tpsDetail(h)}
          </div>
        </div>
        <div class="req-sec">
          <h5>请求瀑布</h5>
          ${waterfallChart(h)}
        </div>
        <div class="req-sec">
          <h5>详情</h5>
          <div class="hint" style="line-height:1.8">
            实际发往上游的模型：<code>${esc(h.effective_model || "-")}</code><br>
            ${h.key ? `实际使用的 Key：<code>${esc(h.key)}</code>（Key 名字）<br>` : ""}
            路由来源：${esc(h.route_source || "-")}　·　${requestAttemptLabel(h, trail)}
            ${h.error || h.status >= 400 ? `<br><span style="color:var(--bad)">错误：${esc(explainHttpError(h.error, h.status))}</span>` : ""}
          </div>
        </div>
        ${trail.length ? `<div class="req-sec">
          <h5>尝试链（依次试过的上游）</h5>
          <div class="hint" style="line-height:1.9">${trail.map(s => {
            const mark = s.ok
              ? `<span style="color:var(--ok)">✓</span>`
              : `<span style="color:var(--bad)">✗</span>`;
            const parts = [esc(s.upstream || "-")];
            if (s.key) parts.push(`<span class="badge">${esc(s.key)}</span>`);
            if (s.status != null) parts.push(String(s.status));
            if (s.model) parts.push(`<code>${esc(s.model)}</code>`);
            if (s.reason || s.status >= 400) parts.push(`<span style="color:var(--bad)">${esc(explainHttpError(s.reason, s.status))}</span>`);
            return `${mark} ${parts.join(" · ")}`;
          }).join("<br>")}</div>
        </div>` : ""}
        ${loadingTxt ? `<div class="req-sec"><div class="hint">正在读取输入输出…</div></div>` : ""}
        ${full && full.reasoning_text ? `<div class="req-sec">
          <h5>思考过程（推理模型）</h5><pre style="color:var(--dim)">${esc(full.reasoning_text)}</pre>
        </div>` : ""}
        ${full && full.request_text ? `<div class="req-sec">
          <h5>输入</h5><pre>${esc(full.request_text)}</pre>
        </div>` : ""}
        ${full && full.response_text ? `<div class="req-sec">
          <h5>输出</h5><pre>${esc(full.response_text)}</pre>
        </div>` : ""}
      </div>` : ""}
    </div>`;
  }).join("");

  // 还有更多就从流水继续取（不是内存上限，能一直往下翻）
  const rest = (journalPage ? journalPage.total : shown.length) - shown.length;
  if (rest > 0) {
    $("#sideReqs").innerHTML += `<div style="text-align:center;padding:6px 0 2px">
      <button class="mini" id="reqMore">加载更多（还有 ${fmtCount(rest)} 条）</button>
    </div>`;
    $("#reqMore").onclick = async () => {
      reqPage += 1;
      await loadJournal();
      lastSideHash = "";
      renderSide();
    };
  }
}

// 展开某条时，按需把它的输入输出正文取回来（列表里不带，省流量）
async function ensureEntryText(at, upstream) {
  const key = String(at);
  if (reqTextCache.has(key)) return;
  reqTextCache.set(key, null);   // 占位，避免重复请求
  try {
    const qs = new URLSearchParams({ at: String(at) });
    if (upstream) qs.set("upstream", upstream);
    const r = await api("/admin/journal/entry?" + qs.toString());
    reqTextCache.set(key, r.entry || {});
  } catch (e) {
    reqTextCache.set(key, { __error: e.message });
  }
  lastSideHash = "";
  renderSide();
}

/* ================= 左侧 tab：请求记录 / 上游来源 / 模型路由 ================= */
const TABS = ["req", "up", "model", "health", "sessions", "tools", "cache", "agent", "ck", "set", "log"];
const TAB_LABEL = { req: "请求记录", up: "上游来源", model: "模型路由", health: "健康看板", sessions: "会话对账", tools: "MCP 工具", cache: "缓存账本", agent: "Agent", ck: "签到", set: "设置", log: "日志" };
let curTab = null;

function setTab(name) {
  // 中继页面没有本地 /agents、/health/upstreams、/sessions 接口，回退到请求记录
  if ((name === "agent" || name === "health" || name === "sessions" || name === "tools" || name === "cache") && !AGENT_LOCAL) name = "req";
  const tab = TABS.includes(name) ? name : "req";
  TABS.forEach(t => {
    const btn = document.querySelector(`#tabs .tab[data-tab="${t}"]`);
    if (!btn) return;
    const on = t === tab;
    btn.classList.toggle("active", on);
    btn.setAttribute("aria-selected", on ? "true" : "false");
    $("#pane-" + t).hidden = !on;
  });
  // 切到日志：清掉「未读」小数字，并刷新一遍
  if (tab === "log") {
    logUnread = 0;
    setTabNum("log", 0);
    renderLog();
  }
  // 切到签到：立刻拉一次表格（不靠 2.5 秒轮询等）
  if (tab === "ck") loadCheckin();
  // 切到本机 Agent：扫一次本机安装情况
  if (tab === "agent") loadAgents();
  // 切到健康看板：拉一次按上游聚合的指标
  if (tab === "health") loadHealth();
  // 切到会话对账：读各家 Agent 会话记录，找没经过本机路由的调用
  if (tab === "sessions") loadSessions();
  // 切到 MCP 工具：单独读工具调用记录
  if (tab === "tools") loadTools();
  // 切到缓存账本：读 Prompt Cache 的读写与命中率
  if (tab === "cache") loadCacheLedger();
  // 换 tab 时回到顶部，免得带着上一屏的滚动位置落进新内容中间
  if (curTab !== null && curTab !== tab) window.scrollTo(0, 0);
  curTab = tab;
  document.title = "llm-router 控制台 · " + TAB_LABEL[tab];
  try { localStorage.setItem("llm-router-tab", tab); } catch (e) {}
  // 切过去立刻按该 tab 需要的精度拉一次（up/model 要 full，req 要 range）。
  if (state) {
    if (tab === "req" || tab === "up" || tab === "model") {
      refresh().catch(() => {});
    } else if (tab === "set") {
      renderSettings();
    }
    renderTabNums();
  }
}

// tab 栏要粘在 header 底下，而 header 窄屏会换行，高度得实测
function syncTabsTop() {
  const h = document.querySelector("header");
  if (h) $("#tabs").style.top = (h.offsetHeight + 12 + (document.body.classList.contains("native-frame") ? 38 : 0)) + "px";
}

/* ================= 无边框窗口：自定义标题栏 ================= */
async function loadWindowChrome() {
  const bar = $("#nativeTitlebar");
  if (!bar) return;
  // 只有 Tauri 桌面壳跳转过来时带 ?native=1；普通浏览器/手机访问不显示伪标题栏
  if (new URLSearchParams(location.search).get("native") !== "1") {
    bar.hidden = true;
    return;
  }
  try {
    const r = await api("/admin/window", undefined, "GET");
    if (!r.native) return;
    document.body.classList.add("native-frame");
    bar.hidden = false;
    const v = $("#tbVer");
    if (v && state && state.version) v.textContent = "v" + state.version;
    syncTabsTop();
  } catch (e) { /* 浏览器直开：不显示自定义标题栏 */ }
}

function windowCommand(path) {
  api(path, {}, "POST").catch(() => {});
}

$("#tbMin").onclick = () => windowCommand("/admin/window/minimize");
$("#tbMax").onclick = () => windowCommand("/admin/window/toggle-maximize");
$("#tbClose").onclick = () => windowCommand("/admin/window/close");
$("#nativeTitlebar").addEventListener("mousedown", e => {
  if (e.button === 0 && !e.target.closest("button")) windowCommand("/admin/window/drag");
});
$("#nativeTitlebar").addEventListener("dblclick", e => {
  if (!e.target.closest("button")) windowCommand("/admin/window/toggle-maximize");
});

// tab 上的小数字：请求条数 / 上游个数 / 已配置路由数
function setTabNum(tab, n, bad, title) {
  const el = $("#tabN-" + tab);
  if (!el) return;
  el.textContent = n ? String(n) : "";
  el.classList.toggle("bad", !!bad);
  if (title) el.title = title;
}

function renderTabNums() {
  const upsAll = state.upstreams || [];
  const nOn = upsAll.filter(u => u.enabled).length;
  const uCount = $("#upCount");
  const modelFindOn = ($("#upModelFind") && $("#upModelFind").value || "").trim();
  if (uCount && !modelFindOn) {
    uCount.textContent = upsAll.length
      ? `${upsAll.length} 个上游，其中 ${nOn} 个已启用` : "";
  }
  // 角标 = 落盘流水里的总条数（跨重启累计，和列表同一个来源）。
  // 优先用流水索引的 total；索引还没读到就退回聚合统计。
  const journalTotal = journalIndex && journalIndex.total != null
    ? journalIndex.total : null;
  const liveAll = journalPage && journalPage.scope === "all" ? journalPage.total : null;
  const allTotals = (state.stats || {}).totals || {};
  const fallback = (state.stats_range || {}).totals || (state.call_stats || {}).totals || {};
  const totals = allTotals.calls != null ? allTotals : fallback;
  const nCalls = liveAll != null ? liveAll
                                  : (journalTotal != null ? journalTotal
                                  : (totals.calls != null ? totals.calls : 0));
  // tab 上的数字是「累计请求数」，不是错误数：失败多少条在 tooltip 里说，
  // 别把整个数字染红，否则 410 会被误读成 410 个错误
  setTabNum("req", nCalls, false,
            `调用记录共 ${nCalls} 条（全部 Agent 会话；可切到本机路由流水）；` +
            `按天 / 按月 / 按分组筛选，` +
            `${totals.failed ? `其中 ${totals.failed} 次失败` : "没有失败"}`);
  const ups = state.upstreams || [];
  setTabNum("up", ups.length, false, `${ups.length} 个上游`);
  const nModels = Object.keys(state.models || {}).length;
  setTabNum("model", nModels, false, `${nModels} 个模型配了路由`);
}

/* ================= 主循环 ================= */
// 统计范围跟着筛选走：选了某天就按天统计，选了某月就按月，否则全量。
// 这样面板上的数字和下面的列表永远是同一个口径。
function statsRangeKey() {
  let day = ($("#sideDay") && $("#sideDay").value) || "";
  const month = ($("#sideMonth") && $("#sideMonth").value) || "";
  // 和 curJournalFilter 同理：首次进入时下拉框还没有「今天」，直接按今天统计。
  if (!day && !month && !journalDayDefaulted) day = dayKeyOf();
  if (day) return "day:" + day;
  if (month) return "month:" + month;
  return "all";
}

// 设置页：全局默认参数（保存后覆盖所有上游，不管之前有没有单独配过）
function renderSettings() {
  const d = (state && state.defaults) || {};
  const f = $("#setFirstTimeout"), c = $("#setConnectTimeout");
  if (!f || !c) return;
  // 正输入着就别覆盖
  if (document.activeElement !== f) {
    f.value = d.first_timeout_seconds != null ? d.first_timeout_seconds : "";
  }
  if (document.activeElement !== c) {
    c.value = d.connect_timeout_seconds != null ? d.connect_timeout_seconds : "";
  }
  // 同一页上的「上游网络出口」：端口、每个上游的实际出口、Clash 片段
  renderProxySettings();
  loadWindowBehavior();
  loadRemoteAccess();
  loadRelay();
  loadTransferSummary();
}

let winBehaviorLoaded = false;
async function loadWindowBehavior(force) {
  const card = $("#winBehaviorCard");
  if (!card || (winBehaviorLoaded && !force)) return;
  try {
    const r = await api("/admin/window", undefined, "GET");
    card.hidden = false;
    $("#winBehavior").value = r.behavior || "tray";
    winBehaviorLoaded = true;
  } catch (e) {
    card.hidden = true;   // 后端不支持时静默隐藏
    winBehaviorLoaded = true;
  }
}

async function saveWindowBehavior() {
  try {
    await api("/admin/window", { behavior: $("#winBehavior").value });
    $("#winBehaviorStatus").textContent = "已保存";
    toast("窗口行为已保存", "ok");
  } catch (e) {
    $("#winBehaviorStatus").textContent = "";
    toast("保存失败：" + e.message, "bad");
  }
}

/* ================= 设置页：远程访问（Tailscale） ================= */
let remoteLoaded = false;

async function loadRemoteAccess(force) {
  const card = $("#remoteCard");
  if (!card || (remoteLoaded && !force)) return;
  try {
    const r = await api("/admin/remote", undefined, "GET");
    card.hidden = false;
    remoteLoaded = true;
    renderRemote(r);
  } catch (e) {
    card.hidden = true;
  }
}

function renderRemote(r) {
  const ts = r.tailscale || {};
  let status;
  if (!ts.installed) {
    status = "Tailscale 未安装：这台电脑和手机都需要安装 Tailscale，并登录同一账号（tailscale.com/download）。";
  } else if (!ts.running) {
    status = "Tailscale 服务未运行：" + (ts.message || "请打开 Tailscale 并登录");
  } else if (!ts.online) {
    status = "Tailscale 已启动但未连接（状态：" + (ts.backend_state || "unknown") + "），请先在 Tailscale 里登录。";
  } else {
    status = "Tailscale 已连接" + (ts.dns_name ? " · " + ts.dns_name : "");
  }
  $("#remoteStatus").textContent = status;

  const enabled = !!r.enabled;
  $("#remoteEnabled").checked = enabled;
  const box = $("#remoteUrls");
  if (!enabled) {
    box.innerHTML = '<div class="hint">开启并重启应用后，这里会显示手机可用的地址。</div>';
    return;
  }
  const urls = r.urls || [];
  box.innerHTML = urls
    .map(u => `<div class="row" style="gap:8px;margin-bottom:6px">
      <code style="flex:1;min-width:0;overflow:auto">${esc(u)}</code>
      <button class="mini" data-copy="${esc(u)}">复制</button>
    </div>`)
    .join("") + '<div class="hint" style="margin-top:6px">手机上打开 Tailscale 后，用上面的地址访问即可。</div>';
  box.querySelectorAll("[data-copy]").forEach(btn => {
    btn.onclick = () => {
      navigator.clipboard.writeText(btn.dataset.copy)
        .then(() => toast("已复制", "ok"))
        .catch(() => {});
    };
  });
}

async function saveRemoteAccess() {
  const btn = $("#remoteSave");
  btn.disabled = true;
  try {
    const r = await api("/admin/remote", { enabled: $("#remoteEnabled").checked });
    $("#remoteSaveStatus").textContent = "已保存" + (r.restart_required ? "，重启应用后生效" : "");
    toast(r.enabled ? "已开启远程访问，重启应用后生效" : "已关闭远程访问，重启应用后生效", "ok");
    await loadRemoteAccess(true);
  } catch (e) {
    $("#remoteSaveStatus").textContent = "";
    toast("保存失败：" + e.message, "bad");
  }
  btn.disabled = false;
}
$("#remoteSave").onclick = saveRemoteAccess;
$("#remoteRefresh").onclick = () => loadRemoteAccess(true);

/* ================= 设置页：边缘中继 ================= */
let relayLoaded = false;

async function loadRelay(force) {
  const card = $("#relayCard");
  if (!card || (relayLoaded && !force)) return;
  try {
    const r = await api("/admin/relay", undefined, "GET");
    card.hidden = false;
    relayLoaded = true;
    if ($("#relayUrl")) $("#relayUrl").value = r.url || "";
    if ($("#relayToken")) $("#relayToken").value = "";
    $("#relayStatus").textContent = r.token_set ? `已配置令牌（${r.token_hint}）` : "令牌未设置";
  } catch (e) {
    card.hidden = true;
  }
}

async function saveRelay() {
  const btn = $("#relaySave");
  btn.disabled = true;
  try {
    await api("/admin/relay", {
      url: $("#relayUrl").value.trim(),
      token: $("#relayToken").value.trim(),
    });
    relayLoaded = false;
    await loadRelay(true);
    toast("边缘中继已保存，桌面端会自动重连", "ok");
  } catch (e) {
    toast("保存失败：" + e.message, "bad");
  }
  btn.disabled = false;
}
$("#relaySave").onclick = saveRelay;

async function saveSettings() {
  const num = el => {
    const raw = (el.value || "").trim();
    if (raw === "") return 0;
    const n = Math.floor(Number(raw));
    return Number.isFinite(n) && n > 0 ? n : 0;
  };
  const btn = $("#setSave");
  btn.disabled = true;
  $("#setStatus").textContent = "\u4fdd\u5b58\u4e2d\u2026";
  try {
    const r = await api("/admin/defaults", {
      first_timeout_seconds: num($("#setFirstTimeout")),
      connect_timeout_seconds: num($("#setConnectTimeout")),
    });
    const d = r.defaults || {};
    $("#setStatus").textContent =
      `\u5df2\u5e94\u7528\u5230 ${r.upstreams || 0} \u4e2a\u4e0a\u6e38\uff08\u9996\u5305 ${d.first_timeout_seconds}s / \u8fde\u63a5 ${d.connect_timeout_seconds}s\uff09`;
    toast("\u9ed8\u8ba4\u53c2\u6570\u5df2\u4fdd\u5b58\u5e76\u5e94\u7528\u5230\u6240\u6709\u4e0a\u6e38", "ok");
    await refresh();
  } catch (e) {
    $("#setStatus").textContent = "";
    toast("\u4fdd\u5b58\u5931\u8d25\uff1a" + e.message, "bad");
  }
  btn.disabled = false;
}
$("#setSave").onclick = saveSettings;
$("#winBehaviorSave").onclick = saveWindowBehavior;

/* ================= 设置页：上游网络出口 ================= */
// 三个本地端口 + 每个上游实际走哪条。测路结果只存在这个浏览器里（lastNetBench），
// 刷新卡片时不会丢，但重新打开页面就没了 —— 它是一次性的测量，不是配置。

let lastNetBench = {};        // 上游 id -> {routes:[{mode,label,ok,ms}], fastest}
let lastNetHtml = null;       // 千篇一律的防抖：内容没变就不碰 DOM

function netModeText(mode) {
  return { direct: "直连", rule: "规则", proxy: "强制代理", custom: "你指定的代理" }[mode] || mode;
}

function renderProxyTable(p) {
  const rows = (p.upstreams || []);
  const html = rows.length ? rows.map(u => {
    const bench = lastNetBench[u.id];
    const fast = bench && bench.fastest ? netModeText(bench.fastest) : "—";
    const benchMs = bench && bench.fastest_ms != null ? Math.round(bench.fastest_ms) + " ms" : "";
    const eff = u.proxy_effective
      ? `<span class="mono" style="font-size:11.5px">${esc(u.proxy_effective)}</span>`
      : `<span class="hint">不显式用代理</span>`;
    const bad = u.proxy_error
      ? `<div class="hint" style="color:var(--bad)">${esc(u.proxy_error)}</div>` : "";
    // 每条路的明细：只在测过之后才有。★ = 最快那条
    const detail = bench
      ? `<div style="padding:2px 0 6px;font-size:11.5px;color:var(--dim)">` +
        (bench.routes || []).map(x => {
          const best = bench.fastest && x.mode === bench.fastest;
          const label = netModeText(x.mode);
          if (!x.ok) {
            return `<span style="margin-right:11px;color:var(--bad)" title="${esc(x.error || "")}">` +
                   `${esc(label)} 失败</span>`;
          }
          if (x.tun_intercepted) {
            // 这条「直连」其实被 TUN 接走了 —— 数字是真的，但它不是另一条路
            return `<span style="margin-right:11px;color:var(--warn)"` +
                   ` title="没配「直连端口」时，这条流量仍被 Clash 的 TUN 接走、按规则出网；` +
                   `这个数字不代表一条独立的直连路径">` +
                   `${esc(label)} ${Math.round(x.ms)}ms（被 TUN 接管）</span>`;
          }
          const spread = (x.min_ms != null && x.max_ms != null &&
                          Math.round(x.max_ms) !== Math.round(x.ms))
            ? `［${Math.round(x.min_ms)}–${Math.round(x.max_ms)}］` : "";
          return `<span style="margin-right:11px;color:${best ? "var(--ok)" : "var(--dim)"}"` +
                 ` title="典型值（中位数）${Math.round(x.ms)}ms，波动 ${spread || "无"}">` +
                 `${best ? "★ " : ""}${esc(label)} ${Math.round(x.ms)}ms${esc(spread)}</span>`;
        }).join("") + `</div>`
      : "";
    // 有上游的「直连」被 TUN 接管时，整行给一句明确的补救说明
    const tunWarn = (bench && bench.blocked_direct)
      ? `<div class="hint" style="color:var(--warn);padding-bottom:6px">` +
        `这个上游的 DNS 是 fake-ip（${esc(bench.host)} → 198.18.x.x），` +
        `「直连」还在被 TUN 接管。配了 Clash 的 <code>proxy: DIRECT</code> 端口并填进「直连端口」后，` +
        `这条才是真的直连。</div>`
      : "";
    return `<div style="padding:6px 0;border-bottom:1px dashed var(--line)">
      <div style="display:flex;gap:10px;align-items:baseline;flex-wrap:wrap">
        <b style="font-size:12.5px;min-width:150px;flex:1 1 150px;overflow:hidden;
          text-overflow:ellipsis;white-space:nowrap">${esc(u.name)}</b>
        <span style="min-width:74px;font-size:12px">${esc(netModeText(u.net_mode_effective))}</span>
        <span style="flex:1 1 160px;min-width:0">${eff}</span>
        <span class="hint" style="min-width:120px;text-align:right">最快 ${esc(fast)}${benchMs ? " · " + benchMs : ""}</span>
      </div>${tunWarn}${detail}${bad}</div>`;
  }).join("") : `<div class="hint">还没有上游。</div>`;

  const table = $("#pxTable");
  if (table && lastNetHtml !== html) { lastNetHtml = html; table.innerHTML = html; }

  const clash = (p.clash || {});
  const pre = $("#pxClash");
  const text = [clash.listeners_yaml || "", "", clash.rules_yaml || ""].join("\n");
  if (pre && document.activeElement !== pre) pre.textContent = text;
  const notes = $("#pxNotes");
  if (notes) notes.innerHTML = (clash.notes || []).map(n => `· ${esc(n)}`).join("<br>");
}

function renderProxySettings() {
  const p = (state && state.proxy) || {};
  const en = $("#pxEnabled");
  if (!en) return;
  if (document.activeElement !== en) en.checked = !!p.enabled;
  const dm = $("#pxDefaultMode");
  if (dm && document.activeElement !== dm) dm.value = p.default_mode || "rule";
  const urls = p.urls || {};
  [["#pxRule", "rule"], ["#pxProxy", "proxy"], ["#pxDirect", "direct"]].forEach(([sel, k]) => {
    const el = $(sel);
    if (el && document.activeElement !== el) el.value = urls[k] || "";
  });
  renderProxyTable(p);
}

async function saveProxySettings() {
  const btn = $("#pxSave");
  btn.disabled = true;
  $("#pxStatus").textContent = "保存中…";
  try {
    const r = await api("/admin/proxy/save", {
      enabled: $("#pxEnabled").checked,
      default_mode: $("#pxDefaultMode").value || "rule",
      urls: {
        rule: ($("#pxRule").value || "").trim(),
        proxy: ($("#pxProxy").value || "").trim(),
        direct: ($("#pxDirect").value || "").trim(),
      },
    });
    const ups = (r.proxy && r.proxy.upstreams || []).length;
    $("#pxStatus").textContent = `已保存，影响 ${ups} 个上游`;
    toast("出口设置已保存", "ok");
    await refresh();
  } catch (e) {
    $("#pxStatus").textContent = "";
    toast("保存失败：" + e.message, "bad");
  }
  btn.disabled = false;
}
$("#pxSave").onclick = saveProxySettings;

// 扫本机端口：把「哪个端口是代理」直接摆出来，还顺手填进规则端口。
// 用户不用去 Clash 里翻端口号 —— 这一步是纯本地探测，不发任何上游请求。
async function detectProxies(fill) {
  const btn = fill ? null : $("#pxDetect");
  if (btn) btn.disabled = true;
  $("#pxStatus").textContent = fill ? "正在扫描并自动填…" : "正在扫描本机端口…";
  try {
    const r = await api("/admin/proxy/detect", fill ? { fill: true } : {});
    if (fill) {
      $("#pxStatus").textContent = (r.filled || []).length
        ? "已填好：" + r.filled.join("；")
        : "规则端口已经有值了，没有改动";
      toast((r.filled || []).length ? "已按实测填好规则端口" : "没有需要填的", "ok");
      await refresh();
      return;
    }
    const found = r.found || [];
    $("#pxStatus").innerHTML = found.length
      ? "发现：" + found.map(f =>
          `<span class="mono" style="margin-right:10px">${esc(f.url)}（${esc(f.kind)}）</span>`).join("") +
        `<button class="mini" id="pxFillNow" type="button" style="margin-left:6px">用这个填规则端口</button>`
      : esc(r.hint || "没发现代理端口");
    const use = document.querySelector("#pxFillNow");
    if (use) use.onclick = () => detectProxies(true);
    if (found.length) toast(`发现 ${found.length} 个本地代理端口`, "ok");
  } catch (e) {
    $("#pxStatus").textContent = "";
    toast("扫描失败：" + e.message, "bad");
  }
  if (btn) btn.disabled = false;
}
$("#pxDetect").onclick = () => detectProxies(false);

async function benchAllRoutes() {
  const btn = $("#pxBench");
  btn.disabled = true;
  $("#pxStatus").textContent = "正在测每条路（握手耗时）…";
  try {
    const r = await api("/admin/net/bench", {});
    (r.results || []).forEach(row => {
      lastNetBench[row.id] = { fastest: row.fastest, fastest_ms: row.fastest_ms,
                               routes: row.routes };
    });
    const n = (r.results || []).length;
    $("#pxStatus").textContent = n
      ? `测完 ${n} 个上游（每条路各测 5 次取中位数，方括号里是最快~最慢）`
      : "没有可测的上游";
    renderProxySettings();
    toast(`测完 ${n} 个上游`, "ok");
  } catch (e) {
    $("#pxStatus").textContent = "";
    toast("测路失败：" + e.message, "bad");
  }
  btn.disabled = false;
}
$("#pxBench").onclick = benchAllRoutes;

async function autoPickRoutes() {
  if (!confirm("按刚才/现在测出的最快路径，把每个上游各自钉到它最快的那条？\n\n" +
               "会真的改写每个上游的出口设置。")) return;
  const btn = $("#pxAuto");
  btn.disabled = true;
  $("#pxStatus").textContent = "正在测路并自动选择…";
  try {
    const r = await api("/admin/net/auto", {});
    (r.results || []).forEach(row => {
      lastNetBench[row.id] = { fastest: row.fastest, fastest_ms: row.fastest_ms,
                               routes: row.routes };
    });
    const changed = r.changed || [];
    const kept = r.kept || [];
    let msg = changed.length
      ? `已改 ${changed.length} 个：` + changed.map(c =>
          `${c.name} ${netModeText(c.from)}→${netModeText(c.to)}`).join("，")
      : "没有需要改的";
    if (kept.length) {
      msg += `；${kept.length} 个保持不动（差距太小，免得抖动来回甩）：` +
        kept.map(k => `${k.name} ${k.reason}`).join("，");
    }
    $("#pxStatus").textContent = msg;
    toast(changed.length ? `按最快路径切换了 ${changed.length} 个上游`
                         : "没有需要改的（差距不到切换门槛）", "ok");
    await refresh();
  } catch (e) {
    $("#pxStatus").textContent = "";
    toast("自动选路失败：" + e.message, "bad");
  }
  btn.disabled = false;
}
$("#pxAuto").onclick = autoPickRoutes;

async function copyClashPart(which) {
  const p = (state && state.proxy) || {};
  const clash = p.clash || {};
  const text = which === "rules" ? (clash.rules_yaml || "") : (clash.listeners_yaml || "");
  try {
    await navigator.clipboard.writeText(text);
    toast(which === "rules" ? "rules 片段已复制" : "listeners 片段已复制", "ok");
  } catch (e) {
    toast("复制失败，手动选一下下面的框：" + e.message, "bad");
  }
}
$("#pxCopyListeners").onclick = () => copyClashPart("listeners");
$("#pxCopyRules").onclick = () => copyClashPart("rules");

$("#logCopy").onclick = async () => {
  const text = logEntries.map(e => {
    const d = new Date(e.at);
    const ts = `${d.getFullYear()}-${pad2(d.getMonth() + 1)}-${pad2(d.getDate())} ` +
               `${pad2(d.getHours())}:${pad2(d.getMinutes())}:${pad2(d.getSeconds())}`;
    return `${ts} [${e.kind || "info"}] ${e.msg}`;
  }).join("\n");
  try {
    await navigator.clipboard.writeText(text);
    toast(`\u5df2\u590d\u5236 ${logEntries.length} \u6761\u65e5\u5fd7`, "ok");
  } catch (e) {
    toast("\u590d\u5236\u5931\u8d25\uff1a" + e.message, "bad");
  }
};

$("#logClear").onclick = () => {
  if (logEntries.length && !confirm("\u6e05\u7a7a\u5168\u90e8\u65e5\u5fd7\uff1f")) return;
  logEntries = [];
  try { localStorage.removeItem(LOG_KEY); } catch (e) {}
  logUnread = 0;
  setTabNum("log", 0);
  renderLog();
};

let catalogRev = null;
let catalogLoading = null;
let journalHydrated = false;

// /stats 不再带 4367 个分组的 catalog；只有版本号变了才重新拉。
async function ensureCatalog() {
  const rev = state && state.catalog_rev;
  if (state.catalog && catalogRev === rev) return;
  if (catalogLoading) {
    await catalogLoading;
    if (state.catalog && catalogRev === rev) return;
  }
  catalogLoading = api("/stats/catalog")
    .then(r => {
      state.catalog = r.catalog || [];
      catalogRev = r.rev;
    })
    .catch(() => {
      // 拉失败就先空着，记下版本避免每 5 秒重试轰炸
      if (!state.catalog) state.catalog = [];
      catalogRev = rev;
    })
    .finally(() => { catalogLoading = null; });
  await catalogLoading;
}

async function refresh() {
  try {
    const prevCatalog = state && state.catalog;
    const firstLoad = !state;
    if (firstLoad && curTab === "req") {
      // 首屏一次拿齐统计 + 流水索引 + 第一页，省掉 3 个串行 RTT。
      const bq = new URLSearchParams({
        full: "0",
        range: statsRangeKey(),
        limit: String(REQ_PAGE),
        offset: "0",
      });
      const boot = await api("/bootstrap?" + bq.toString());
      state = boot.stats || {};
      journalIndex = boot.journal_index || null;
      journalPage = boot.journal || null;
      journalHydrated = true;
    } else {
      const wantFull = curTab === "up" || curTab === "model";
      let statsUrl = "/stats?full=" + (wantFull ? "1" : "0");
      if (curTab === "req") statsUrl += "&range=" + encodeURIComponent(statsRangeKey());
      state = await api(statsUrl);
      if (prevCatalog) state.catalog = prevCatalog;
    }
    // 服务端版本变了（刚重启过）就自动重载页面 —— 否则浏览器还跑着旧的
    // 前端 JS、后端已是新版本，界面上就会出现「两边对不上」的假象。
    if (pageVersion && state.version && state.version !== pageVersion) {
      pageVersion = state.version;
      location.reload();
      return;
    }
    pageVersion = state.version;
    const tbVer = $("#tbVer");
    if (tbVer) tbVer.textContent = "v" + state.version;
    $("#health").className = "dot on";
    $("#ver").textContent = "v" + state.version;
    $("#baseUrl").textContent = serverUrl + "/v1";
    $("#statePath").textContent = state.state_path || "";
    // catalog 只在模型/上游 tab 需要；其他 tab 不拉 4367 个分组。
    if (curTab === "model" || curTab === "up") await ensureCatalog();
    if (curTab === "up") renderUpstreams();
    if (curTab === "model") renderModels();
    if (curTab === "set") renderSettings();
    // 签到页：只在这一页上才刷（读的是后端本地状态，不产生上游请求）
    if (curTab === "ck") loadCheckin();
    // 请求记录：只有这一页才读流水；其他 tab 完全不碰 8MB 历史文件。
    if (curTab === "req") {
      if (!journalHydrated) {
        await loadJournalIndex();
        fillFilterOptions();
        await loadJournal();
      } else {
        journalHydrated = false;
        fillFilterOptions();
      }
      const hash = JSON.stringify([
        journalPage ? journalPage.total : null,
        journalPage ? (journalPage.entries || []).length : 0,
        ($("#sideDay") ? $("#sideDay").value : ""),
        ($("#sideMonth") ? $("#sideMonth").value : ""),
        ($("#sideFilter") ? $("#sideFilter").value : ""),
        ($("#sideUpstream") ? $("#sideUpstream").value : ""),
        ($("#sideMode") ? $("#sideMode").value : ""),
        (state.stats_range || {}).totals,
        reqPage,
      ]);
      if (hash !== lastSideHash) {
        lastSideHash = hash;
        renderSide();
      }
    }
    if (curTab === "req") loadReqCharts();
    renderTabNums();
    refreshDelay = refreshSeconds() * 1000;
  } catch (e) {
    $("#health").className = "dot off";
    // 服务端挂了/网络断了：指数退避，别每 5 秒无脑重试。
    refreshDelay = Math.min(Math.max(refreshDelay * 2, 5000), 60000);
  }
}

/* ================= 事件绑定 ================= */
// 左侧 tab 栏
document.querySelectorAll("#tabs .tab").forEach(b => {
  b.onclick = () => setTab(b.dataset.tab);
});
$("#sideRefresh").onclick = async () => { lastSideHash = ""; await refresh(); };
if ($("#sideSource")) {
  $("#sideSource").onchange = async () => {
    reqPage = 1; lastSideHash = "";
    await loadJournal();
    renderSide();
  };
}
if ($("#sideMetric")) {
  $("#sideMetric").onchange = () => {
    reqPage = 1; lastSideHash = "";
    saveTimingMode($("#sideMetric").value);
  };
}
// 按天 / 按月 / 按分组筛选：改动后回到第一页重新从流水取
if ($("#sideDay")) {
  $("#sideDay").onchange = async () => {
    if ($("#sideDay").value && $("#sideMonth")) $("#sideMonth").value = "";
    reqPage = 1; lastSideHash = "";
    await loadJournal();
    renderSide();
  };
}
if ($("#sideMonth")) {
  $("#sideMonth").onchange = async () => {
    if ($("#sideMonth").value && $("#sideDay")) $("#sideDay").value = "";
    reqPage = 1; lastSideHash = "";
    await loadJournal();
    renderSide();
  };
}
$("#sideFilter").onchange = async () => {
  // 换了分组，上一个分组选中的上游多半不属于新分组，先清掉再查，
  // 否则会带着旧上游把新分组筛成空。
  if ($("#sideUpstream")) $("#sideUpstream").value = "";
  reqPage = 1; lastSideHash = "";
  await loadJournal();
  renderSide();
};
if ($("#sideUpstream")) {
  $("#sideUpstream").onchange = async () => {
    reqPage = 1; lastSideHash = "";
    await loadJournal();
    renderSide();
  };
}
$("#sideMode").onchange = () => { lastSideHash = ""; renderSide(); };
$("#snapBtn").onclick = () => {
  $("#historyModal").hidden = false;
  refreshHistoryModal();
};
$("#historyClose").onclick = () => { $("#historyModal").hidden = true; };
$("#historyModal").addEventListener("click", e => {
  if (e.target === $("#historyModal")) $("#historyModal").hidden = true;
});
// 点请求行展开/收起详情（事件委托，列表重建也不丢）。
// 展开时若正文还没取过，按需去流水里取这一条的完整内容（列表不带正文）。
$("#sideReqs").addEventListener("click", async e => {
  const head = e.target.closest(".req-head");
  if (!head) return;
  const key = head.parentElement.dataset.key;
  const opening = !openReqs.has(key);
  if (opening) openReqs.add(key);
  else openReqs.delete(key);
  lastSideHash = "";          // 强制重建
  renderSide();
  if (opening && !reqTextCache.has(key)) {
    const row = ((journalPage && journalPage.entries) || [])
      .find(x => String(x.at) === key);
    if (row) await ensureEntryText(row.at, row.upstream);
  }
});

$("#addBtn").onclick = addUpstream;
// 上游来源搜索：输入即筛(名字 / 地址 / 勾选模型的真实名 / 分组名)
if ($("#upSearch")) $("#upSearch").addEventListener("input", renderUpstreams);
if ($("#upModelFind")) $("#upModelFind").addEventListener("input", renderUpstreams);
if ($("#upEnabledFilter")) $("#upEnabledFilter").addEventListener("change", renderUpstreams);
["newName", "newUrl", "newKey"].forEach(id => {
  $("#" + id).addEventListener("keydown", e => { if (e.key === "Enter") addUpstream(); });
});

// 全部启用 / 全部停用
async function setAllUpstreams(enabled) {
  const ups = (state.upstreams || []).filter(u => u.enabled !== enabled);
  if (!ups.length) { toast(enabled ? "所有上游都已启用" : "所有上游都已停用"); return; }
  try {
    for (const u of ups) {
      await api("/admin/upstream/update", { id: u.id, enabled });
    }
    toast(`${enabled ? "已启用" : "已停用"} ${ups.length} 个上游`, "ok");
    await refresh();
  } catch (e) { toast("操作失败：" + e.message, "bad"); await refresh(); }
}
$("#enableAllBtn").onclick = () => setAllUpstreams(true);
$("#disableAllBtn").onclick = () => setAllUpstreams(false);
$("#upSortAsc").onclick = () => { upSortDir = 1; renderUpstreams(); };
$("#upSortDesc").onclick = () => { upSortDir = -1; renderUpstreams(); };
$("#autoBtn").onclick = autoConfigure;

// 「拉取全部模型」「测速」「记录与快照」三个顶部按钮已去掉：
// 前两个在每个上游卡片里都有对应的按钮（要哪个点哪个，比全量更可控），
// 「记录与快照」的内容并进了「请求记录」页（快照回退入口也放在那里）。

async function refreshHistoryModal() {
  const filesBox = $("#journalFiles");
  const versBox = $("#stateVersions");
  const logBox = $("#adminLogList");
  if (!filesBox || !versBox) return;
  filesBox.innerHTML = '<div class="dim">加载中…</div>';
  versBox.innerHTML = '<div class="dim">加载中…</div>';
  if (logBox) logBox.innerHTML = '<div class="dim">加载中…</div>';
  try {
    const [files, versions, logs] = await Promise.all([
      api("/admin/journal/files"),
      api("/admin/state/versions"),
      api("/admin/admin-log?limit=50").catch(() => ({ log: [] })),
    ]);
    const j = (state && state.journal) || {};
    const fl = files.files || [];
    filesBox.innerHTML = `
      <div class="dim" style="margin-bottom:6px">
        累计 ${fmtNum(j.current_lines || 0)} 行未归档 ·
        共 ${fl.length} 个文件 · ${fmtBytes(j.bytes || 0)}
        ${j.rotate_at ? ` · 当前文件到 ${fmtNum(j.rotate_at)} 行自动归档` : ""}
      </div>
      ${fl.length ? fl.map(f => `
        <div class="hist-row"><code>${esc(f.path)}</code>
          <span class="dim">${fmtBytes(f.size)}</span></div>`).join("")
        : '<div class="dim">还没有流水文件</div>'}
      <div class="dim" style="margin-top:8px">
        流水只增不减：归档是改名留存，不会被删掉。
      </div>`;

    const vs = versions.versions || [];
    versBox.innerHTML = vs.length ? vs.map((v, i) => `
      <div class="hist-row">
        <code>${esc(v.file)}</code>
        <span class="dim">${v.upstreams != null ? v.upstreams + " 上游 / " : ""}${v.models != null ? v.models + " 模型" : ""}</span>
        <button class="mini" data-restore="${esc(v.file)}" ${i === 0 ? "disabled" : ""}>
          ${i === 0 ? "当前" : "回退到此"}</button>
      </div>`).join("") : '<div class="dim">还没有快照</div>';

    versBox.querySelectorAll("[data-restore]").forEach(btn => {
      btn.onclick = async () => {
        const file = btn.dataset.restore;
        if (!confirm(`回退到这个快照？\n\n${file}\n\n当前配置会先存成一份新快照，所以这一步也能退回来。`)) return;
        btn.disabled = true;
        try {
          const r = await api("/admin/state/restore", { file });
          toast(`已回退：${r.upstreams} 个上游、${r.models} 个模型`, "ok");
          await refresh();
          await refreshHistoryModal();
        } catch (e) { toast("回退失败：" + e.message, "bad"); btn.disabled = false; }
      };
    });

    if (logBox) {
      const lg = logs.log || [];
      logBox.innerHTML = lg.length ? lg.map(e => `
        <div class="hist-row"><span class="dim">${esc(e.time || "")}</span>
          <code>${esc(e.op || "")}</code>
          <span class="dim">${esc(e.actor || "")}</span>
          <span class="hist-detail">${esc(briefDetail(e.detail))}</span></div>`).join("")
        : '<div class="dim">还没有管理操作记录</div>';
    }
  } catch (e) {
    filesBox.innerHTML = `<div class="dim">读取失败：${esc(e.message)}</div>`;
    versBox.innerHTML = "";
  }
}

function briefDetail(d) {
  if (!d || typeof d !== "object") return "";
  return Object.entries(d)
    .map(([k, v]) => `${k}=${Array.isArray(v) ? v.length + "项" : String(v).slice(0, 40)}`)
    .join("  ");
}

function fmtBytes(n) {
  n = Number(n) || 0;
  if (n < 1024) return n + " B";
  if (n < 1024 * 1024) return (n / 1024).toFixed(1) + " KB";
  return (n / 1024 / 1024).toFixed(1) + " MB";
}

$("#accessBtn").onclick = () => { $("#accessModal").hidden = false; };
$("#accessClose").onclick = () => { $("#accessModal").hidden = true; };
$("#accessModal").addEventListener("click", e => {
  if (e.target === $("#accessModal")) $("#accessModal").hidden = true;
});

$("#copyBtn").onclick = async () => {
  const url = serverUrl + "/v1";
  try { await navigator.clipboard.writeText(url); toast("已复制：" + url, "ok"); }
  catch (e) { toast("复制失败，请手动选中：" + url, "bad"); }
};

$("#mFilter").addEventListener("input", () => { modelLimit = 200; renderModels(); });
$("#mFilterMode").addEventListener("change", () => { modelLimit = 200; renderModels(); });
$("#mRename").onclick = async () => {
  if (!editing) return;
  const from = editing.name;
    const to = (prompt(`把客户端调用的名字「${from}」改成什么？\n\n` +
                       `只改你在客户端里填的 model 名；各上游那边的真实模型名不变，` +
                       `它们的映射会自动指到新名字上。`, from) || "").trim();
  if (!to || to === from) return;
  try {
    const r = await api("/admin/rename-group", { from, to });
    editing = null;
    selectedModel = to;
    toast(`分组已改名为「${to}」，涉及 ${r.entries} 个上游模型`, "ok");
    await refresh();
    openModel(to);
  } catch (e) { toast("改名失败：" + e.message, "bad"); }
};
$("#eClose").onclick = () => { $("#editModal").hidden = true; editId = null; };
$("#editModal").addEventListener("click", e => {
  if (e.target === $("#editModal")) { $("#editModal").hidden = true; editId = null; }
});
$("#eSave").onclick = saveUpstreamEdit;
["eName", "eUrl"].forEach(id => {
  $("#" + id).addEventListener("keydown", e => { if (e.key === "Enter") saveUpstreamEdit(); });
});
document.addEventListener("keydown", e => {
  if (e.key !== "Escape") return;
  if (!document.getElementById("groupMenu").hidden) { closeGroupMenu(); return; }
  if (!$("#accessModal").hidden) { $("#accessModal").hidden = true; return; }
  if (!$("#historyModal").hidden) { $("#historyModal").hidden = true; return; }
  if (!$("#editModal").hidden) { $("#editModal").hidden = true; editId = null; }
});
// 点别处、改窗口大小都收起分组下拉
document.addEventListener("mousedown", e => {
  if (document.getElementById("groupMenu").hidden) return;
  if (e.target.closest("#groupMenu") || e.target.closest("input[data-group-for]")) return;
  closeGroupMenu();
});
// 滚动时不要让菜单消失，而是跟着输入框一起移动。
// （之前用捕获模式直接 close，导致用户为了看到输入框一滚动，菜单就被关掉了）
window.addEventListener("scroll", () => {
  if (!groupMenuFor || document.getElementById("groupMenu").hidden) return;
  // 输入框已经滚出可视区域才收起
  const r = groupMenuFor.getBoundingClientRect();
  if (r.bottom < 0 || r.top > window.innerHeight) { closeGroupMenu(); return; }
  positionGroupMenu(groupMenuFor);
}, true);
window.addEventListener("resize", () => {
  syncTabsTop();
  if (!groupMenuFor) return;
  if (document.getElementById("groupMenu").hidden) return;
  positionGroupMenu(groupMenuFor);
});
document.querySelectorAll("#mStrategy button").forEach(b => {
  b.onclick = () => { if (editing) { editing.strategy = b.dataset.s; renderModal(); } };
});
if ($("#mAffinity")) $("#mAffinity").onchange = () => {
  if (!editing) return;
  editing.affinity = $("#mAffinity").value;
  if ($("#mAffinityHint")) {
    const am = editing.affinity;
    $("#mAffinityHint").textContent = am === "auto" ? "默认：有缓存收益才固定"
      : am === "off" ? "完全不固定" : am === "strict" ? "只认上次成功的上游" : "按会话/单轮固定";
  }
};
$("#mSave").onclick = saveModel;
$("#mDelete").onclick = deleteModel;
$("#mAddBtn").onclick = () => {
  const id = $("#mAddUpstream").value;
  if (!id) return;
  const u = (state.upstreams || []).find(x => x.id === id);
  if (!u) return;
  // 尽量用「这个上游归到该分组」的真实模型名，找不到就用分组名本身
  const reals = (u.selected_models || []).filter(m => (u.model_groups || {})[m] === editing.name);
  const real = reals[0]
    || (u.available_models || []).find(m => (u.model_groups || {})[m] === editing.name)
    || editing.name;
  editing.picks.push({
    id, name: u.name,
    hasModel: (u.available_models || []).includes(real),
    model: real, enabled: true, quota: "",
  });
  renderModal();
};

/* ---- 具名排序的按钮 ---- */
$("#mSortSel").onchange = () => {
  if (editing) editing.sortLabel = $("#mSortSel").value;
  renderSortBox();
};
$("#mSortSave").onclick = () => {
  if (!editing) return;
  // 直接弹出输入行，并带一个默认名字，改一个字就能存
  $("#mSortNew").hidden = false;
  const inp = $("#mSortName");
  inp.value = editing.sortLabel && editing.sortLabel !== "原始"
    ? editing.sortLabel : "按测速";
  inp.focus();
  inp.select();
};
$("#mSortSaveNew").onclick = () => saveOrdering($("#mSortName").value);
$("#mSortCancelNew").onclick = () => {
  $("#mSortNew").hidden = true;
  $("#mSortName").value = "";
};
$("#mSortName").addEventListener("keydown", e => {
  if (e.key === "Enter") saveOrdering($("#mSortName").value);
  else if (e.key === "Escape") $("#mSortCancelNew").onclick();
});
$("#mSortApply").onclick = () => applyOrdering($("#mSortSel").value);
$("#mSortDelete").onclick = () => deleteOrdering($("#mSortSel").value);
$("#mSortBySpeed").onclick = reorderBySpeed;

/* ================= 签到 ================= */
// 后端：GET /admin/checkin 一串拿齐（rows + settings + summary + recent），
// POST /admin/checkin/run 执行，/config 存单个上游的令牌，/settings 存自动签到。
let ckData = null;      // 最近一次拿到的签到数据
let ckEditing = "";     // 正在编辑令牌的上游 id：编辑时暂停自动刷新，别把输入刷掉

const CK_STYLE = {
  ok:      { cls: "ok",   text: "签到成功" },
  already: { cls: "ok",   text: "今天已签" },
  blocked: { cls: "warn", text: "需人机验证" },
  auth:    { cls: "bad",  text: "令牌无效" },
  error:   { cls: "bad",  text: "失败" },
  none:    { cls: "",     text: "无签到接口" },
  nocfg:   { cls: "",     text: "未配置令牌" },
  unknown: { cls: "",     text: "未检测" },
};

function ckRowState(r) {
  const st = r.state || null;
  let kind, title;
  if (!r.configured) {
    kind = "nocfg";
    title = "还没填访问令牌，点「令牌」配置";
  } else if (!st) {
    kind = "unknown";
    title = "还没签过，点「签到」试一次";
  } else {
    kind = st.kind || "unknown";
    title = st.message || "";
    if (st.at) title += "（" + new Date(st.at * 1000).toLocaleString() + "）";
  }
  return { kind, title, st, meta: CK_STYLE[kind] || CK_STYLE.unknown };
}

function renderCheckin() {
  const box = $("#ckRows");
  if (!box) return;
  const rows = (ckData && ckData.rows) || [];
  if (!rows.length) {
    applyHtml("checkin", box, '<div class="hint">还没有上游。</div>');
    return;
  }

  const html = rows.map(r => {
    const { title, st, meta } = ckRowState(r);

    // ---- 正在编辑令牌：换成输入框 ----
    if (ckEditing === r.id) {
      return `<div class="ck-row" data-id="${esc(r.id)}">
        <div class="ck-name">
          <b>${esc(r.name)}</b>
          <span class="hint mono">${esc(r.site_root)}</span>
        </div>
        <div class="ck-main">
          <input class="mono" data-ck="token" type="password" autocomplete="off"
                 placeholder="访问令牌（站点「个人设置 → 访问令牌」）"
                 value="" style="width:100%">
          <input class="mono" data-ck="uid" placeholder="User id（老版本才要，可空）"
                 value="${esc(r.user_id || "")}" style="width:100%;margin-top:6px">
        </div>
        <div class="ck-act">
          <button class="mini primary" data-ck-act="save">保存</button>
          <button class="mini" data-ck-act="cancel">取消</button>
        </div>
      </div>`;
    }

    // ---- 常规行 ----
    const reward = (st && st.reward_text)
      ? `<span class="hint" title="这次签到拿到的额度">+${esc(st.reward_text)}</span>` : "";
    const wrong = r.token_looks_wrong
      ? `<span class="badge bad" title="这里要填站点的访问令牌，不是 sk- 开头的推理 Key">填成了 sk-</span>`
      : "";
    const hintBadge = r.configured
      ? `<span class="badge mono" title="访问令牌（打码）">${esc(r.token_hint)}</span>` : "";
    const expiredBadge = r.token_expired
      ? `<span class="badge bad" title="JWT 已过期，请到站点重新生成访问令牌后保存">令牌已过期</span>` : "";
    const partBox = r.configured
      ? `<label class="hint" style="display:flex;align-items:center;gap:5px"
           title="关掉后「一键签到」和自动签到都会跳过这个上游">
           <input type="checkbox" data-ck-act="toggle" ${r.checkin_enabled ? "checked" : ""}> 参与</label>`
      : "";

    return `<div class="ck-row" data-id="${esc(r.id)}">
      <div class="ck-name">
        <b>${esc(r.name)}</b>
        <span class="hint mono">${esc(r.site_root)}</span>
      </div>
      <div class="ck-state">
        <span class="badge ${meta.cls}" title="${esc(title)}"
          ${meta.cls ? "" : 'style="color:var(--dim)"'}>${esc(meta.text)}</span>
        ${reward}${wrong}${expiredBadge}${hintBadge}
      </div>
      <div class="ck-act">
        ${partBox}
        <button class="mini" data-ck-act="edit">令牌</button>
        <button class="mini primary" data-ck-act="run"
          ${r.configured ? "" : "disabled"}>签到</button>
      </div>
    </div>`;
  }).join("");

  applyHtml("checkin", box, html);
}

function renderCkSummary() {
  const s = (ckData && ckData.summary) || {};
  const rows = (ckData && ckData.rows) || [];
  const configured = rows.filter(r => r.configured).length;
  const el = $("#ckSummary");
  if (el) {
    el.textContent = `已配置令牌 ${configured} / ${rows.length} 个上游` +
      ` · 今天已签 ${s.today_checked || 0}`;
  }
  const n = $("#tabN-ck");
  if (n) n.textContent = s.today_checked ? String(s.today_checked) : "";
}

function renderCkHistory() {
  const box = $("#ckHistory");
  if (!box) return;
  const recent = (ckData && ckData.recent) || [];
  if (!recent.length) {
    box.className = "hint";
    box.textContent = "还没有记录。签到过一次之后这里会留下流水。";
    return;
  }
  box.className = "";
  box.innerHTML = recent.slice(0, 60).map(e => {
    const meta = CK_STYLE[e.kind] || CK_STYLE.unknown;
    const at = e.at ? new Date(e.at * 1000).toLocaleString() : "";
    const reward = e.reward_text ? ` <span class="hint">+${esc(e.reward_text)}</span>`
      : (e.reward_usd ? ` <span class="hint">+$${esc(e.reward_usd)}</span>` : "");
    const msg = e.message ? `<span class="hint" style="flex:1;min-width:0;
      overflow:hidden;text-overflow:ellipsis;white-space:nowrap">${esc(e.message)}</span>` : "";
    return `<div class="ck-hist-row">
      <span class="t">${esc(at)}</span>
      <b style="white-space:nowrap">${esc(e.name || e.id || "")}</b>
      <span class="badge ${meta.cls}" ${meta.cls ? "" : 'style="color:var(--dim)"'}>${esc(meta.text)}</span>
      ${reward}${msg}
    </div>`;
  }).join("");
}

function renderCkSettings() {
  const s = (ckData && ckData.settings) || {};
  const setIfIdle = (sel, val) => {
    const el = $(sel);
    // 用户正把光标放在这个框里就别覆盖他打了一半的内容
    if (el && document.activeElement !== el) el.value = val;
  };
  const on = $("#ckAutoOn");
  if (on && document.activeElement !== on) on.checked = !!s.enabled;
  setIfIdle("#ckAutoAt", s.at || "08:00");
  setIfIdle("#ckAutoWin", s.random_minutes == null ? 30 : s.random_minutes);
}

async function loadCheckin(force) {
  if (!force && ckEditing) return;      // 正在编辑令牌，别把输入刷掉
  try {
    ckData = await api("/admin/checkin?limit=200");
  } catch (e) {
    return;                             // 后端旧版本没有这个接口时安静跳过
  }
  renderCheckin();
  renderCkSummary();
  renderCkHistory();
  renderCkSettings();
}

// ids 为空 = 所有配了令牌且勾了「参与」的上游
async function runCheckin(ids, probe) {
  const body = {};
  if (ids && ids.length) body.ids = ids;
  if (probe) body.probe = true;
  const label = probe ? "探测" : "签到";
  toast(`正在${label}…`);
  try {
    const res = await api("/admin/checkin/run", body);
    if (res.summary) ckData = Object.assign({}, ckData || {}, {
      summary: res.summary, recent: res.recent });
    await loadCheckin(true);
    const list = res.results || [];
    const good = list.filter(r => r.ok).length;
    const blocked = list.filter(r => r.kind === "blocked").length;
    let msg = `${label}完成：${good}/${list.length} 成功`;
    if (blocked) msg += `，${blocked} 个需要人机验证`;
    toast(msg, good ? "" : "bad");
  } catch (e) {
    toast(`${label}失败：` + e.message, "bad");
  }
}

async function saveCkConfig(id, token, userId) {
  try {
    await api("/admin/checkin/config", { id, token: token, user_id: userId });
    ckEditing = "";
    await loadCheckin(true);
    toast("已保存");
  } catch (e) {
    toast("保存失败：" + e.message, "bad");
  }
}

async function toggleCkPart(id, on) {
  try {
    // "__keep__" = 只改开关，别动已经存好的令牌
    await api("/admin/checkin/config", { id, token: "__keep__", enabled: on });
    await loadCheckin(true);
  } catch (e) {
    toast("保存失败：" + e.message, "bad");
  }
}

async function saveCkSettings() {
  const body = {
    enabled: $("#ckAutoOn").checked,
    at: ($("#ckAutoAt").value || "").trim() || "08:00",
    random_minutes: Number($("#ckAutoWin").value || 0),
  };
  try {
    const res = await api("/admin/checkin/settings", body);
    await loadCheckin(true);
    const s = res.settings || body;
    toast(s.enabled
      ? `已开启：每天 ${s.at} 起 ${s.random_minutes} 分钟内随机签到`
      : "已关闭自动签到");
  } catch (e) {
    toast("保存失败：" + e.message, "bad");
  }
}

// 签到页的事件：行内按钮用委托，整页按钮直接绑
$("#ckRows").addEventListener("click", e => {
  const btn = e.target.closest("button[data-ck-act]");
  if (!btn || btn.disabled) return;
  const row = btn.closest(".ck-row");
  if (!row) return;
  const id = row.dataset.id;
  const act = btn.dataset.ckAct;
  if (act === "run") runCheckin([id], false);
  else if (act === "edit") { ckEditing = id; renderCheckin(); }
  else if (act === "cancel") { ckEditing = ""; renderCheckin(); }
  else if (act === "save") {
    const token = (row.querySelector('[data-ck="token"]') || {}).value || "";
    const uid = (row.querySelector('[data-ck="uid"]') || {}).value || "";
    if (!token.trim()) { toast("令牌不能为空（想停用这个上游就取消勾选「参与」）", "bad"); return; }
    saveCkConfig(id, token.trim(), uid.trim());
  }
});
$("#ckRows").addEventListener("change", e => {
  const cb = e.target.closest('input[data-ck-act="toggle"]');
  if (!cb) return;
  const row = cb.closest(".ck-row");
  if (row) toggleCkPart(row.dataset.id, cb.checked);
});
$("#ckRunAll").onclick = () => runCheckin(null, false);
$("#ckProbeAll").onclick = () => runCheckin(null, true);
$("#ckAutoSave").onclick = saveCkSettings;

/* ================= 界面刷新间隔 ================= */
const REFRESH_KEY = "llm-router-refresh-seconds";
const TIMING_KEY = "llm-router-timing-metric";
const SESS_REFRESH_KEY = "llm-router-sess-refresh-seconds";
let refreshTimer = null;
let refreshDelay = 5000;
let refreshing = false;

// 首字 / 耗时口径：happycoding（默认）/ magpie / dsh。
// 后端按这个参数换算法重算，不改原始时间戳。
const TIMING_TEXT = {
  happycoding: "happycoding（New API）：首字=第一个 SSE 帧（含空帧），耗时=开始处理→写回完成并截断到整秒；起点在读完请求体之后，数字最小。",
  magpie: "Magpie：首字=上游第一个有内容的字节（跳过空帧），耗时=到最后一个字节，毫秒精度；会话记录里没有「网关收包」时刻，所以按会话 + token 指纹去 Magpie 自己的用量日志里对账，对上的直接用它的 ttft_ms / ms，对不上的回退到 DSH request/header 时刻。",
  dsh: "DSH 会话（当前）：首字=第一个流帧（从 step/start 算起），耗时=step/start→assistant/message 落盘，毫秒精度；含 DSH 序列化时间（实测中位比网关多 1.8 秒）。",
};
function timingMode() {
  const v = localStorage.getItem(TIMING_KEY);
  return v === "magpie" || v === "dsh" ? v : "happycoding";
}
function applyTimingMode() {
  const v = timingMode();
  const sel = $("#timingMode");
  if (sel) sel.value = v;
  const quick = $("#sideMetric");
  if (quick) quick.value = v;
  const note = $("#timingNote");
  if (note) note.innerHTML = "<b>当前口径：</b>" + esc(TIMING_TEXT[v]);
  const st = $("#timingStatus");
  if (st) st.textContent = "";
}
async function saveTimingMode(v) {
  localStorage.setItem(TIMING_KEY, v === "magpie" || v === "dsh" ? v : "happycoding");
  applyTimingMode();
  const st = $("#timingStatus");
  if (st) st.textContent = "已切换，正在按新口径重新统计…";
  // 请求记录与会话对账都要重新取数，refresh() 只刷统计，不刷这两份明细。
  lastSideHash = "";
  if (curTab === "req") {
    await loadJournal();
    renderSide();
  }
  if (curTab === "sessions") await loadSessions();
  await refresh();
  if (st) st.textContent = "已按新口径重算";
  toast("已切换为 " + timingMode() + " 口径", "ok");
}

function refreshSeconds() {
  const raw = Number(localStorage.getItem(REFRESH_KEY));
  return Number.isFinite(raw) && raw >= 1 && raw <= 120 ? Math.round(raw) : 5;
}

function sessRefreshSeconds() {
  const raw = Number(localStorage.getItem(SESS_REFRESH_KEY));
  return Number.isFinite(raw) && raw >= 2 && raw <= 600 ? Math.round(raw) : 5;
}

function scheduleRefresh(delay) {
  if (refreshTimer) clearTimeout(refreshTimer);
  refreshTimer = null;
  if (document.hidden) return; // 窗口最小化/托盘/切后台：完全不发请求
  // 长连接开着时，轮询只做兜底（30 秒一次）；增量由 WS 推。
  const live = ws && ws.readyState === 1;
  const wait = live ? Math.max(delay, 30000) : Math.max(1000, delay);
  refreshTimer = setTimeout(async () => {
    if (refreshing) return;
    refreshing = true;
    try { await refresh(); } finally {
      refreshing = false;
      scheduleRefresh(refreshDelay);
    }
  }, wait);
}

function applyRefreshInterval() {
  const secs = refreshSeconds();
  const input = $("#refreshInterval");
  if (input && document.activeElement !== input) input.value = String(secs);
  const sessInput = $("#sessRefreshInterval");
  if (sessInput && document.activeElement !== sessInput) sessInput.value = String(sessRefreshSeconds());
  refreshDelay = secs * 1000;
  scheduleRefresh(refreshDelay);
}

function saveRefreshInterval() {
  const input = $("#refreshInterval");
  const n = Math.round(Number((input.value || "").trim()));
  if (!Number.isFinite(n) || n < 1 || n > 120) {
    $("#refreshIntervalStatus").textContent = "请求记录间隔请输入 1–120 之间的整数";
    return;
  }
  const sessInput = $("#sessRefreshInterval");
  const sn = Math.round(Number((sessInput.value || "").trim()));
  if (!Number.isFinite(sn) || sn < 2 || sn > 600) {
    $("#refreshIntervalStatus").textContent = "会话对账间隔请输入 2–600 之间的整数";
    return;
  }
  localStorage.setItem(REFRESH_KEY, String(n));
  localStorage.setItem(SESS_REFRESH_KEY, String(sn));
  applyRefreshInterval();
  if (typeof applySessAuto === "function") applySessAuto();
  if (typeof applyToolsAuto === "function") applyToolsAuto();
  $("#refreshIntervalStatus").textContent = `已保存：请求记录每 ${n} 秒，会话对账每 ${sn} 秒`;
  toast(`刷新间隔已保存（请求记录 ${n}s / 会话对账 ${sn}s）`, "ok");
}
$("#refreshIntervalSave").onclick = saveRefreshInterval;
if ($("#timingMode")) {
  $("#timingMode").onchange = () => saveTimingMode($("#timingMode").value);
}
applyTimingMode();

const UPDATE_AUTO_KEY = "llm-router-update-auto";
const UPDATE_CHANNEL_KEY = "llm-router-update-channel";
function updateAutoEnabled() {
  return localStorage.getItem(UPDATE_AUTO_KEY) !== "0";
}
async function checkUpdate(manual) {
  const channel = ($("#updateChannel") && $("#updateChannel").value) || "stable";
  const status = $("#updateStatus");
  const box = $("#updateResult");
  if (status) status.textContent = "检查中…";
  try {
    const d = await api("/update/check?channel=" + encodeURIComponent(channel));
    if (status) status.textContent = d.latest ? (d.has_update ? `发现 ${d.latest}` : `已是最新 · ${d.current}`) : "暂无 release";
    if (!box) return;
    if (!d.latest) {
      box.innerHTML = `<div class="hint">当前 ${esc(d.current || "")}；仓库还没有可用的 release。</div>`;
      return;
    }
    const assets = (d.assets || []).map(a => `<a class="badge" href="${esc(a.url)}" target="_blank" rel="noreferrer">${esc(a.name)} · ${(Number(a.size || 0) / 1048576).toFixed(1)} MB</a>`).join(" ");
    const notes = (d.notes || "").trim();
    box.innerHTML = `<div class="card" style="margin:0;padding:12px">
      <div class="row" style="gap:8px;align-items:center">
        <strong>${esc(d.latest)}</strong>
        <span class="badge" style="color:${d.has_update ? "var(--ok)" : "var(--dim)"}">${d.has_update ? "有新版本" : "已是最新"}</span>
        <span class="badge">${esc(d.channel)}</span>
        <span class="hint">当前 ${esc(d.current)} · ${esc(d.published_at || "")}</span>
      </div>
      ${notes ? `<pre class="mono" style="max-height:180px;overflow:auto;white-space:pre-wrap;background:var(--bg);padding:8px;border:1px solid var(--line);border-radius:6px;margin-top:8px">${esc(notes.slice(0, 4000))}</pre>` : ""}
      <div class="row" style="gap:8px;margin-top:8px;flex-wrap:wrap">${assets || `<a class="badge" href="${esc(d.html_url)}" target="_blank" rel="noreferrer">打开 release 页</a>`}</div>
    </div>`;
    if (manual && d.has_update) toast(`发现新版本 ${d.latest}`, "ok");
  } catch (e) {
    if (status) status.textContent = "检查失败";
    if (box) box.innerHTML = `<div class="hint" style="color:var(--bad)">${esc(e.message || e)}</div>`;
  }
}
function applyUpdateSettings() {
  const ch = localStorage.getItem(UPDATE_CHANNEL_KEY) || "stable";
  const sel = $("#updateChannel");
  if (sel) sel.value = ch === "beta" ? "beta" : "stable";
  const auto = $("#updateAuto");
  if (auto) auto.checked = updateAutoEnabled();
  if (updateAutoEnabled()) checkUpdate(false);
}
if ($("#updateCheck")) $("#updateCheck").onclick = () => checkUpdate(true);
if ($("#updateChannel")) $("#updateChannel").onchange = () => {
  localStorage.setItem(UPDATE_CHANNEL_KEY, $("#updateChannel").value);
  checkUpdate(false);
};
if ($("#updateAuto")) $("#updateAuto").onchange = () => {
  localStorage.setItem(UPDATE_AUTO_KEY, $("#updateAuto").checked ? "1" : "0");
  if ($("#updateAuto").checked) checkUpdate(false);
};
setTimeout(applyUpdateSettings, 800);

async function loadCollectorStatus() {
  const el = $("#collectorStatus");
  if (!el) return;
  try {
    const d = await api("/collector/status");
    const entries = (d.entries || []).length;
    el.textContent = `运行 ${fmtCount(d.runs || 0)} 轮 · 命中 ${fmtCount(d.hits || 0)} · 未命中 ${fmtCount(d.misses || 0)} · 缓存 ${entries} 项 · 间隔 ${d.interval_seconds || 10}s`;
  } catch (e) {
    el.textContent = "读取失败：" + (e.message || e);
  }
}
if ($("#collectorRefresh")) $("#collectorRefresh").onclick = loadCollectorStatus;
setTimeout(loadCollectorStatus, 1200);

document.addEventListener("visibilitychange", () => {  if (document.hidden) {
    if (refreshTimer) clearTimeout(refreshTimer);
    refreshTimer = null;
  } else {
    refreshDelay = refreshSeconds() * 1000;
    if (!refreshing) {
      refreshing = true;
      refresh().finally(() => {
        refreshing = false;
        scheduleRefresh(refreshDelay);
      });
    }
  }
});

/* ================= WebSocket：增量推送，取代轮询 ================= */
let ws = null;
let wsRetry = null;
let eventDebounce = null;

function startSocket() {
  if (ws && (ws.readyState === 0 || ws.readyState === 1)) return;
  try {
    const proto = location.protocol === "https:" ? "wss://" : "ws://";
    ws = new WebSocket(proto + location.host + "/ws");
  } catch (e) {
    ws = null;
    wsRetry = setTimeout(startSocket, 10000);
    return;
  }
  ws.onmessage = (ev) => {
    try { handleSocket(JSON.parse(ev.data)); } catch (e) {}
  };
  ws.onclose = () => {
    ws = null;
    clearTimeout(wsRetry);
    wsRetry = setTimeout(startSocket, 8000);
  };
  ws.onerror = () => { try { ws.close(); } catch (e) {} };
}

function handleSocket(m) {
  if (!m) return;
  // 新记录直接推过来，前端 prepend，不再拉列表
  if (m.type === "journal" && m.entry) {
    applyJournalDelta(m.entry);
    return;
  }
  // 配置/统计变化：轻量刷一次
  if (m.type === "state") {
    clearTimeout(eventDebounce);
    eventDebounce = setTimeout(() => { refresh().catch(() => {}); }, 300);
    return;
  }
  if (m.type === "update" && m.payload && m.payload.has_update) {
    const p = m.payload;
    toast(`发现新版本 ${p.latest}（${p.channel}）`, "ok");
    const status = $("#updateStatus");
    if (status) status.textContent = `发现 ${p.latest}`;
  }
}

function dayKeyOfTs(at) {
  const n = Number(at || 0);
  if (!n) return "";
  const d = new Date(n * 1000);
  const p = (x) => String(x).padStart(2, "0");
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())}`;
}

function applyJournalDelta(entry) {
  if (!journalPage || curTab !== "req") return;
  const f = curJournalFilter();
  const day = dayKeyOfTs(entry.at);
  if (f.day && day && day !== f.day) return;
  if (f.month && !day.startsWith(f.month)) return;
  if (f.group && (entry.model || "") !== f.group) return;
  if (f.upstream && (entry.upstream || "") !== f.upstream) {
    const inTrail = (entry.trail || []).some((s) => s.upstream === f.upstream);
    if (!inTrail) return;
  }
  const key = String(entry.at);
  const list = journalPage.entries || [];
  if (list.some((x) => String(x.at) === key)) return;
  journalPage.entries = [entry, ...list].slice(0, 200);
  journalPage.total = (journalPage.total || 0) + 1;
  lastSideHash = "";
  renderSide();
  renderTabNums();
}

/* ================= 本机 Agent 接入 ================= */
let agentData = null;
let agentSelected = null;
let agentLoading = false;
let agentImporting = false;

function agentResultHtml(inner) {
  const box = $("#agentResult");
  if (box) box.innerHTML = inner;
}

async function loadAgents() {
  if (!AGENT_LOCAL || agentLoading) return;
  const list = $("#agentList");
  if (!list) return;
  agentLoading = true;
  agentResultHtml('<div class="hint muted">正在扫描 Agent…</div>');
  try {
    const data = await api("/agents", undefined, "GET");
    agentData = data;
    const models = data.models || [];
    const sel = $("#agentModel");
    const prev = sel.value;
    sel.innerHTML = models.map(m => `<option value="${esc(m)}">${esc(m)}</option>`).join("");
    if (prev && models.includes(prev)) sel.value = prev;
    else if (models.length) sel.value = models[0];
    const base = $("#agentBaseUrl");
    if (base && !base.dataset.touched) base.value = data.base_url || "";
    const key = $("#agentApiKey");
    if (key && !key.dataset.touched && !key.value) key.value = "llm-router";
    if (!agentSelected) {
      const first = (data.agents || []).find(a => a.detected && a.configurable)
                 || (data.agents || []).find(a => a.configurable);
      if (first) agentSelected = first.id;
    }
    renderAgents();
    const all = data.agents || [];
    const n = all.filter(a => a.detected).length;
    const auto = all.filter(a => (a.source || "").includes("npm 全局包")).length;
    const canWrite = all.filter(a => a.detected && a.configurable).length;
    setTabNum("agent", n, false, `识别到 ${n} 个 Agent`);
    agentResultHtml(`<div class="hint muted">扫描于 ${esc(data.scan_at || "")} · 本机端口 ${esc(data.port || "")} · 当前路由 ${models.length} 个模型 · 共 ${all.length} 个候选（npm 包动态发现 ${auto} 个）· 已识别且可一键导入 ${canWrite} 个</div>`);
  } catch (e) {
    agentResultHtml(`<div class="hint" style="color:var(--bad)">扫描失败：${esc(e.message || e)}</div>`);
  } finally {
    agentLoading = false;
  }
}

function renderAgents() {
  const list = $("#agentList");
  if (!list || !agentData) return;
  const q = (($("#agentSearch") && $("#agentSearch").value) || "").trim().toLowerCase();
  const onlyDetected = !$("#agentOnlyDetected") || $("#agentOnlyDetected").checked;
  const all = agentData.agents || [];
  const agents = all.filter(a => {
    if (onlyDetected && !a.detected) return false;
    if (!q) return true;
    const hay = [a.id, a.name, a.kind, a.pkg, a.version, a.bin, a.source, a.config_path,
                 (a.config_paths || []).join(" "), (a.detected_by || []).join(" ")]
                .filter(Boolean).join(" ").toLowerCase();
    return hay.includes(q);
  });
  if (!agents.length) {
    list.innerHTML = `<div class="hint muted">没有匹配的 Agent（共扫描 ${all.length} 个）。试试清空搜索或取消「只看已识别」。</div>`;
    return;
  }
  list.innerHTML = agents.map(a => {
    const detected = !!a.detected;
    const sel = agentSelected === a.id;
    const driftBadge = a.drifted
      ? '<span class="badge" style="color:var(--bad)" title="配置被别的工具改走或不再指向本机路由">配置已漂移</span>'
      : (a.expected_model ? '<span class="badge" style="color:var(--ok)">配置一致</span>' : "");
    const badges = [
      driftBadge,
      a.source ? `<span class="badge">${esc(a.source)}</span>` : "",
      a.pkg ? `<span class="badge mono">${esc(a.pkg)}${a.version ? "@" + esc(a.version) : ""}</span>` : "",
      a.bin ? `<span class="badge mono">${esc(a.bin)}</span>` : "",
    ].concat((a.detected_by || []).map(x => `<span class="badge">${esc(x)}</span>`)).filter(Boolean).join(" ");
    const paths = (a.config_paths && a.config_paths.length > 1) ? a.config_paths : [a.config_path];
    const cur = [];
    if (a.current_model) cur.push(`模型 <code>${esc(a.current_model)}</code>`);
    if (a.current_base) cur.push(`地址 <code>${esc(a.current_base)}</code>`);
    if (a.connected) cur.push('<span style="color:var(--ok)">已指向 llm-router</span>');
    if (a.default_provider || a.default_model) {
      const routed = a.default_provider === "llmrouter";
      cur.push(`默认模型 <code style="${routed ? "color:var(--ok)" : "color:var(--bad)"}">${esc((a.default_provider || "?") + "/" + (a.default_model || "?"))}</code>${routed ? "" : "（没走本机路由）"}`);
    }
    if (a.expected_model) cur.push(`上次导入 <code>${esc(a.expected_model)}</code>`);
    if (a.applied_at) cur.push(`<span class="muted">${esc(a.applied_at)}</span>`);
    const notes = a.notes ? `<div class="hint muted" style="margin-top:4px">${esc(a.notes)}</div>` : "";
    const actions = a.configurable
      ? `<div class="row" style="gap:8px;align-items:center;margin-top:8px">
           <label class="hint" style="display:flex;gap:6px;align-items:center">
             <input type="radio" name="agentPick" data-act="pick" data-id="${esc(a.id)}" ${sel ? "checked" : ""}> 选中
           </label>
           <button data-act="import" data-id="${esc(a.id)}" ${detected ? "" : "disabled"} title="${detected ? "一键写入该 Agent 配置" : "未检测到安装"}">一键导入此 Agent</button>
           ${a.drifted ? `<button class="primary" data-act="repair" data-id="${esc(a.id)}" title="用上次导入的模型与地址写回">一键修回</button>` : ""}
         </div>`
      : '<div class="hint muted" style="margin-top:8px">仅识别，请用「复制通用配置」手动填写</div>';
    return `<div class="card" data-agent="${esc(a.id)}" style="margin:0 0 10px;${sel ? "border-color:var(--accent)" : ""}">
      <div class="row" style="gap:8px;align-items:center">
        <strong>${esc(a.name)}</strong>
        <span class="badge" style="${detected ? "color:var(--ok)" : ""}">${detected ? "已识别" : "未检测到"}</span>
        <span class="badge">${esc(a.kind)}</span>
        <div class="grow"></div>
        ${badges}
      </div>
      <div class="hint mono" style="margin-top:6px">${paths.map(p => esc(p)).join("<br>")}</div>
      ${cur.length ? `<div class="hint" style="margin-top:4px">${cur.join(" · ")}</div>` : ""}
      ${notes}
      ${actions}
    </div>`;
  }).join("");
}

async function importAgent(id, dryRun) {
  if (!AGENT_LOCAL || agentImporting) return;
  const model = $("#agentModel") && $("#agentModel").value;
  if (!model) { toast("请先在当前路由里配置模型", "bad"); return; }
  const apiKey = (($("#agentApiKey") && $("#agentApiKey").value) || "llm-router").trim() || "llm-router";
  const baseUrl = (($("#agentBaseUrl") && $("#agentBaseUrl").value) || "").trim();
  const pinDefault = !!($("#agentPinDefault") && $("#agentPinDefault").checked);
  agentImporting = true;
  try {
    const r = await api("/agents/configure", {
      agent_id: id, model, api_key: apiKey, base_url: baseUrl, dry_run: !!dryRun, pin_default: pinDefault
    }, "POST");
    const keys = (r.changed_keys || []).map(k => `<code>${esc(k)}</code>`).join("、");
    const targetPaths = (r.config_paths && r.config_paths.length) ? r.config_paths : [r.config_path];
    const backupPaths = (r.backup_paths && r.backup_paths.length)
      ? r.backup_paths : (r.backup_path ? [r.backup_path] : []);
    const backup = backupPaths.length
      ? `<div class="hint" style="margin-top:4px">备份：<code>${backupPaths.map(esc).join("</code>、<code>")}</code></div>` : "";
    const tx = r.transaction || {};
    const txSteps = (tx.steps || []).map(s => {
      const bad = s.ok === false && !s.skipped;
      const skipped = s.skipped ? "（跳过）" : "";
      const color = bad ? "var(--bad)" : s.skipped ? "var(--dim)" : "var(--ok)";
      return `<span class="badge" style="color:${color};border-color:${bad ? "var(--bad)" : "var(--line)"}">${esc(s.name)}${skipped}${bad ? " ✗" : s.skipped ? "" : " ✓"}</span>`;
    }).join(" → ");
    const txDiffs = (tx.diff || tx.files || []).filter(d => d && d.path);
    const txDetail = txDiffs.length ? `<details style="margin-top:6px"><summary class="hint">变更 diff（${txDiffs.length} 个文件）</summary><div class="hint" style="margin-top:4px">${txDiffs.map(d => `<div><code>${esc(d.path)}</code> · <span style="color:var(--ok)">+${Number(d.added_lines || 0)}</span> / <span style="color:var(--bad)">-${Number(d.removed_lines || 0)}</span></div>`).join("")}</div></details>` : "";
    const preview = r.preview != null
      ? `<pre class="mono" style="max-height:260px;overflow:auto;background:var(--bg);padding:8px;border:1px solid var(--line);border-radius:6px;white-space:pre-wrap">${esc(r.preview)}</pre>` : "";
    agentResultHtml(`<div class="card" style="margin:0">
      <div class="row" style="gap:8px;align-items:center">
        <strong>${esc(r.name || id)}</strong>
        <span class="badge" style="color:${r.dry_run ? "var(--accent)" : "var(--ok)"}">${r.dry_run ? "预览（未写文件）" : "已写入"}</span>
        <span class="badge">${esc(r.model)}</span>
      </div>
      <div class="hint" style="margin-top:6px">写入位置：<code>${targetPaths.map(esc).join("</code>、<code>")}</code></div>
      ${keys ? `<div class="hint" style="margin-top:4px">写入键：${keys}</div>` : ""}
      ${tx.id ? `<div class="hint" style="margin-top:6px">事务 <code>${esc(tx.id)}</code> · ${txSteps}</div>` : ""}
      ${txDetail}
      ${r.pinned_default ? `<div class="hint" style="margin-top:4px">已把 DSH 默认模型钉到 <code>llmrouter/${esc(r.model)}</code>（改的是 ~/.dsh/profiles/*/cordis.patch.yml 的 agent-default-model），重启 DeepSeek Harness 后生效。</div>` : ""}
      ${backup}
      ${preview}
    </div>`);
    toast(r.dry_run ? "已生成预览（未写入）" : "配置已写入，可用「恢复原配置」还原", r.dry_run ? "" : "ok");
    if (!r.dry_run) await loadAgents();
  } catch (e) {
    agentResultHtml(`<div class="hint" style="color:var(--bad)">导入失败：${esc(e.message || e)}</div>`);
    toast("导入失败：" + (e.message || e), "bad");
  } finally {
    agentImporting = false;
  }
}

$("#agentRescan").onclick = () => loadAgents();
$("#agentBaseUrl").addEventListener("input", e => { e.target.dataset.touched = "1"; });
$("#agentApiKey").addEventListener("input", e => { e.target.dataset.touched = "1"; });
$("#agentModel").addEventListener("change", () => renderAgents());
$("#agentSearch").addEventListener("input", () => renderAgents());
$("#agentOnlyDetected").addEventListener("change", () => renderAgents());
$("#agentImport").onclick = () => {
  const fallback = agentData && (agentData.agents || []).find(a => a.detected && a.configurable);
  const id = agentSelected || (fallback && fallback.id);
  if (!id) { toast("请先选中一个可配置的 Agent", "bad"); return; }
  importAgent(id, $("#agentDryRun").checked);
};
$("#agentList").addEventListener("click", ev => {
  const el = ev.target.closest("[data-act]");
  if (!el) {
    const card = ev.target.closest("[data-agent]");
    if (!card) return;
    const radio = card.querySelector('input[data-act="pick"]');
    if (radio) {
      radio.checked = true;
      agentSelected = radio.dataset.id;
      renderAgents();
    }
    return;
  }
  const id = el.dataset.id;
  if (!id) return;
  if (el.dataset.act === "pick") {
    agentSelected = id;
    renderAgents();
  } else if (el.dataset.act === "import") {
    agentSelected = id;
    importAgent(id, $("#agentDryRun").checked);
  } else if (el.dataset.act === "repair") {
    agentSelected = id;
    repairAgent(id);
  }
});

async function repairAgent(id) {
  if (!AGENT_LOCAL || agentImporting) return;
  agentImporting = true;
  try {
    const r = await api("/agents/repair", { agent_id: id }, "POST");
    agentResultHtml(`<div class="card" style="margin:0">
      <div class="row" style="gap:8px;align-items:center">
        <strong>${esc(r.name || id)}</strong>
        <span class="badge" style="color:var(--ok)">已修回</span>
        <span class="badge">${esc(r.model)}</span>
      </div>
      <div class="hint" style="margin-top:6px">写入位置：<code>${((r.config_paths && r.config_paths.length) ? r.config_paths : [r.config_path]).map(esc).join("</code>、<code>")}</code></div>
    </div>`);
    toast("配置已修回", "ok");
    await loadAgents();
  } catch (e) {
    agentResultHtml(`<div class="hint" style="color:var(--bad)">修回失败：${esc(e.message || e)}</div>`);
    toast("修回失败：" + (e.message || e), "bad");
  } finally {
    agentImporting = false;
  }
}
$("#agentCopy").onclick = async () => {
  const model = $("#agentModel").value || (agentData && agentData.models && agentData.models[0]) || "";
  const key = (($("#agentApiKey").value || "llm-router").trim()) || "llm-router";
  let base = (($("#agentBaseUrl").value || (agentData && agentData.base_url) || "http://127.0.0.1:8010/v1").trim());
  while (base.endsWith("/")) base = base.slice(0, -1);
  const origin = base.endsWith("/v1") ? base.slice(0, -3) : base;
  const openaiBase = base.endsWith("/v1") ? base : base + "/v1";
  const text = [
    "# OpenAI 兼容 Agent",
    `export OPENAI_BASE_URL="${openaiBase}"`,
    `export OPENAI_API_KEY="${key}"`,
    `export OPENAI_MODEL="${model}"`,
    "",
    "# Claude Code / Anthropic 协议 Agent",
    `export ANTHROPIC_BASE_URL="${origin}"`,
    `export ANTHROPIC_AUTH_TOKEN="${key}"`,
    `export ANTHROPIC_MODEL="${model}"`,
    `export ANTHROPIC_SMALL_FAST_MODEL="${model}"`,
  ].join("\n");
  try {
    await navigator.clipboard.writeText(text);
    toast("通用配置已复制到剪贴板", "ok");
  } catch (e) {
    agentResultHtml(`<pre class="mono" style="background:var(--bg);padding:8px;border:1px solid var(--line);border-radius:6px;white-space:pre-wrap">${esc(text)}</pre>`);
    toast("剪贴板不可用，已显示在结果区", "bad");
  }
};
$("#agentRestore").onclick = async () => {
  if (!agentSelected) { toast("请先选中一个 Agent", "bad"); return; }
  try {
    const r = await api("/agents/restore", { agent_id: agentSelected }, "POST");
    const restored = (r.restored_files && r.restored_files.length) ? r.restored_files : [r.config_path];
    agentResultHtml(`<div class="card" style="margin:0">
      <div class="row" style="gap:8px"><strong>${esc(r.agent_id)}</strong>
        <span class="badge" style="color:var(--ok)">已恢复 ${restored.length} 个文件</span></div>
      <div class="hint" style="margin-top:6px">从 <code>${esc(r.restored_from)}</code> 还原：<code>${restored.map(esc).join("</code>、<code>")}</code></div>
    </div>`);
    toast("原配置已恢复", "ok");
    await loadAgents();
  } catch (e) {
    agentResultHtml(`<div class="hint" style="color:var(--bad)">恢复失败：${esc(e.message || e)}</div>`);
    toast("恢复失败：" + (e.message || e), "bad");
  }
};
if (!AGENT_LOCAL) {
  const b = document.querySelector('#tabs .tab[data-tab="agent"]');
  if (b) b.style.display = "none";
}

/* ================= 设置页：勾选式导入 / 导出 ================= */
let transferLoaded = false;
let transferPayload = null;

function transferParts() {
  return Array.from(document.querySelectorAll("#transferParts input[type=checkbox]"))
    .filter(x => x.checked).map(x => x.dataset.part);
}
function transferResultHtml(inner) { $("#transferResult").innerHTML = inner; }

async function loadTransferSummary(force) {
  if (!AGENT_LOCAL || (transferLoaded && !force)) return;
  try {
    const r = await api("/data/summary", undefined, "GET");
    const c = r.counts || {};
    $("#transferSummary").textContent =
      `当前：请求 ${c.journal ?? 0} 条 / 上游 ${c.upstreams ?? 0} 个 / ` +
      `路由 ${c.models ?? 0} 条 / 签到 ${c.checkin_entries ?? 0} 条`;
    transferLoaded = true;
  } catch (e) {
    const card = $("#transferCard");
    if (card) card.style.display = "none";   // 中继页 / 后端不支持时静默隐藏
    transferLoaded = true;
  }
}

function transferFilename() {
  const d = new Date(), p = n => String(n).padStart(2, "0");
  return `llm-router-backup-${d.getFullYear()}${p(d.getMonth() + 1)}${p(d.getDate())}` +
    `-${p(d.getHours())}${p(d.getMinutes())}${p(d.getSeconds())}.json`;
}
function downloadJson(obj, name) {
  const blob = new Blob([JSON.stringify(obj, null, 2)], { type: "application/json" });
  const url = URL.createObjectURL(blob);
  const a = document.createElement("a");
  a.href = url; a.download = name;
  document.body.appendChild(a); a.click(); a.remove();
  setTimeout(() => URL.revokeObjectURL(url), 30000);
}
async function buildTransferBundle() {
  const parts = transferParts();
  if (!parts.length) throw new Error("至少勾选一项");
  return await api("/data/export", {
    parts,
    journal_limit: Math.max(0, Number($("#transferJournalLimit").value) || 0),
    mask_secrets: $("#transferMask").checked
  }, "POST");
}
function transferCountsHtml(bundle) {
  const c = bundle.counts || {};
  return `请求 ${c.journal ?? 0} 条 / 上游 ${c.upstreams ?? 0} 个 / ` +
    `路由 ${c.models ?? 0} 条 / 签到 ${c.checkin_entries ?? 0} 条` +
    (bundle.mask_secrets ? "（Key 已打码）" : "");
}

$("#transferExport").onclick = async () => {
  const btn = $("#transferExport");
  btn.disabled = true; btn.textContent = "导出中…";
  try {
    const bundle = await buildTransferBundle();
    downloadJson(bundle, transferFilename());
    transferResultHtml(`<div class="hint">已导出 <b>${esc((bundle.parts || []).join("、"))}</b>：` +
      `${transferCountsHtml(bundle)}</div>`);
    toast("备份已导出", "ok");
  } catch (e) {
    transferResultHtml(`<div class="hint" style="color:var(--bad)">导出失败：${esc(e.message || e)}</div>`);
    toast("导出失败：" + (e.message || e), "bad");
  } finally {
    btn.disabled = false; btn.textContent = "导出 JSON";
  }
};

$("#transferCopy").onclick = async () => {
  try {
    const bundle = await buildTransferBundle();
    const text = JSON.stringify(bundle, null, 2);
    if (navigator.clipboard && navigator.clipboard.writeText) {
      await navigator.clipboard.writeText(text);
      toast("备份 JSON 已复制到剪贴板", "ok");
    } else {
      transferResultHtml(`<pre class="mono" style="background:var(--bg);padding:8px;border:1px solid var(--line);border-radius:6px;white-space:pre-wrap;max-height:320px;overflow:auto">${esc(text)}</pre>`);
      toast("剪贴板不可用，已显示在结果区", "bad");
    }
  } catch (e) {
    toast("复制失败：" + (e.message || e), "bad");
  }
};

$("#transferFile").onchange = async () => {
  const f = $("#transferFile").files[0];
  transferPayload = null;
  if (!f) { transferResultHtml(""); return; }
  try {
    transferPayload = JSON.parse(await f.text());
    const parts = Array.isArray(transferPayload.parts)
      ? transferPayload.parts
      : Object.keys(transferPayload.data || {});
    transferResultHtml(`<div class="hint">已读取 <code>${esc(f.name)}</code>` +
      `（${(f.size / 1024).toFixed(1)} KiB），含：${esc(parts.join("、"))}。` +
      `点「预览变更」先看会改什么，再点「导入」。</div>`);
  } catch (e) {
    transferResultHtml(`<div class="hint" style="color:var(--bad)">文件解析失败：${esc(e.message || e)}</div>`);
  }
};

function transferAppliedHtml(r) {
  const a = r.applied || {};
  const rows = Object.keys(a).map(k =>
    `<div class="row" style="gap:8px"><code style="min-width:150px">${esc(k)}</code>` +
    `<span>${esc(typeof a[k] === "boolean" ? (a[k] ? "是" : "否") : String(a[k]))}</span></div>`
  ).join("");
  const backups = (r.backups || []).map(b => `<code>${esc(b)}</code>`).join("<br>");
  return `<div class="card" style="margin:0">
    <div class="row" style="gap:8px"><strong>${r.dry_run ? "预览（未写入）" : "导入完成"}</strong>
      <span class="badge" style="color:${r.dry_run ? "var(--warn)" : "var(--ok)"}">${esc(r.mode || "")}</span>
      ${r.legacy ? '<span class="badge">旧版 migrate.py 包</span>' : ""}</div>
    <div class="hint" style="margin:6px 0">类别：${esc((r.parts || []).join("、"))}</div>
    ${rows}
    ${backups ? `<div class="hint" style="margin-top:8px">写入前已备份：<br>${backups}</div>` : ""}
  </div>`;
}

async function doTransferImport(dryRun) {
  if (!transferPayload) { toast("请先选择备份 JSON 文件", "bad"); return; }
  const parts = transferParts();
  if (!parts.length) { toast("至少勾选一项", "bad"); return; }
  if (!dryRun && !confirm(`确认${$("#transferMode").value === "replace" ? "覆盖" : "合并"}导入：${parts.join("、")}？\n写入前会自动备份原文件。`)) return;
  try {
    const r = await api("/data/import", {
      payload: transferPayload,
      parts,
      mode: $("#transferMode").value,
      dry_run: dryRun
    }, "POST");
    transferResultHtml(transferAppliedHtml(r));
    toast(dryRun ? "预览完成，未写入" : "导入完成", "ok");
    if (!dryRun) {
      await loadTransferSummary(true);
      try { await refresh(); } catch (e) {}
    }
  } catch (e) {
    transferResultHtml(`<div class="hint" style="color:var(--bad)">导入失败：${esc(e.message || e)}</div>`);
    toast("导入失败：" + (e.message || e), "bad");
  }
}
$("#transferPreview").onclick = () => doTransferImport(true);
$("#transferImport").onclick = () => doTransferImport(false);
if (!AGENT_LOCAL) {
  const card = $("#transferCard");
  if (card) card.style.display = "none";
}

/* ================= 图表：纯 SVG，无第三方依赖 ================= */
const CHART_COLORS = { blue: "#4f9cf9", purple: "#c084fc", green: "#3fb950", warn: "#d29922", red: "#f85149", cyan: "#38bdf8" };
let reqChartAt = 0;
let reqChartBusy = false;

function chartEmpty(host, msg) {
  if (!host) return;
  host._lineKey = ""; host._barKey = ""; host._radarKey = "";
  const text = msg || "没有数据";
  if (host._emptyText === text && host.querySelector(".chart-empty")) return;
  host._emptyText = text;
  host.innerHTML = `<div class="chart-empty">${esc(text)}</div>`;
}

function chartAxis(v) {
  const n = Number(v) || 0;
  if (n >= 1000000) return (n / 1000000).toFixed(n % 1000000 ? 1 : 0) + "M";
  if (n >= 1000) return (n / 1000).toFixed(n % 1000 ? 1 : 0) + "k";
  return String(Math.round(n * 10) / 10);
}
function chartMax(values) {
  const max = Math.max(0, ...values.map(v => Number(v) || 0));
  if (!max) return 1;
  const p = Math.pow(10, Math.floor(Math.log10(max)));
  const n = max / p;
  const m = n <= 1 ? 1 : n <= 2 ? 2 : n <= 5 ? 5 : 10;
  return m * p;
}
function chartMs(ms) {
  const v = Number(ms);
  if (!Number.isFinite(v) || v <= 0) return "—";
  return v < 1000 ? Math.round(v) + "ms" : (v / 1000).toFixed(2) + "s";
}
function shortLabel(s, n) {
  const t = String(s == null ? "" : s);
  return t.length > n ? t.slice(0, n - 1) + "…" : t;
}
function chartLegend(series) {
  return `<div class="chart-legend">${series.map(s => `<span><i style="background:${s.color}"></i>${esc(s.name)}</span>`).join("")}</div>`;
}

function lineChart(host, labels, series, opts = {}) {
  if (!host) return;
  const n = labels.length;
  if (!n || !series.length) { chartEmpty(host); return; }
  const W = 640, H = opts.height || 190;
  const pad = { l: 44, r: 12, t: 12, b: 24 };
  const iw = W - pad.l - pad.r, ih = H - pad.t - pad.b;
  const key = `${n}|${H}|${series.map(s => `${s.name}\u0001${s.color || ""}`).join("|")}`;
  let svg = host.querySelector("svg.chart-svg");
  if (!svg || host._lineKey !== key) {
    host._lineKey = key;
    let grid = "";
    for (let g = 0; g <= 4; g++) {
      const gy = pad.t + (ih * g) / 4;
      grid += `<line x1="${pad.l}" x2="${W - pad.r}" y1="${gy}" y2="${gy}" stroke="rgba(255,255,255,.07)" stroke-width="1"/>`;
      grid += `<text class="chart-grid-label" data-g="${g}" x="${pad.l - 6}" y="${gy + 3}" font-size="9" fill="#8b9bb0" text-anchor="end"></text>`;
    }
    let paths = "";
    series.forEach((s, si) => {
      const color = s.color || CHART_COLORS.blue;
      if (si === 0) paths += `<path fill="${color}" fill-opacity=".10" class="chart-area"/>`;
      paths += `<path fill="none" stroke="${color}" stroke-width="2" stroke-linejoin="round" stroke-linecap="round" class="chart-line" data-s="${si}"/>`;
      s.values.forEach((_, i) => {
        paths += `<circle r="2.5" fill="${color}" class="chart-dot" data-s="${si}" data-i="${i}"><title></title></circle>`;
      });
    });
    let xl = "";
    const step = Math.max(1, Math.ceil(n / 8));
    labels.forEach((lab, i) => {
      if (i % step !== 0 && i !== n - 1) return;
      xl += `<text class="chart-x-label" data-i="${i}" y="${H - 6}" font-size="9" fill="#8b9bb0" text-anchor="middle"></text>`;
    });
    host.innerHTML = chartLegend(series) + `<svg class="chart-svg" viewBox="0 0 ${W} ${H}" role="img">${grid}${paths}${xl}</svg>`;
    svg = host.querySelector("svg");
  }
  const max = chartMax(series.flatMap(s => s.values));
  const x = i => n === 1 ? pad.l + iw / 2 : pad.l + (i * iw) / (n - 1);
  const y = v => pad.t + ih - (Math.max(0, Number(v) || 0) / max) * ih;
  svg.querySelectorAll(".chart-grid-label").forEach((el, g) => {
    el.textContent = chartAxis(max - (max * g) / 4);
  });
  series.forEach((s, si) => {
    const pts = s.values.map((v, i) => `${x(i).toFixed(1)},${y(v).toFixed(1)}`);
    const line = svg.querySelector(`.chart-line[data-s="${si}"]`);
    if (line) line.setAttribute("d", `M ${pts.join(" L ")}`);
    if (si === 0) {
      const area = `M ${x(0).toFixed(1)} ${(pad.t + ih).toFixed(1)} L ${pts.join(" L ")} L ${x(n - 1).toFixed(1)} ${(pad.t + ih).toFixed(1)} Z`;
      const a = svg.querySelector(".chart-area");
      if (a) a.setAttribute("d", area);
    }
    s.values.forEach((v, i) => {
      const c = svg.querySelector(`.chart-dot[data-s="${si}"][data-i="${i}"]`);
      if (!c) return;
      c.setAttribute("cx", x(i).toFixed(1));
      c.setAttribute("cy", y(v).toFixed(1));
      let t = c.querySelector("title");
      if (!t) { t = document.createElementNS("http://www.w3.org/2000/svg", "title"); c.appendChild(t); }
      t.textContent = `${labels[i]} · ${s.name}: ${chartAxis(v)}`;
    });
  });
  labels.forEach((lab, i) => {
    const el = svg.querySelector(`.chart-x-label[data-i="${i}"]`);
    if (el) { el.setAttribute("x", x(i).toFixed(1)); el.textContent = lab; }
  });
}

function waterfallChart(h) {
  const total = Number(h && h.elapsed_seconds) || 0;
  if (!(total > 0)) return `<div class="hint">这条请求没有耗时记录。</div>`;
  const trail = Array.isArray(h.trail) ? h.trail.filter(Boolean) : [];
  const first = Number(h.first_chunk_seconds);
  const tps = tpsOf(h);
  const attempts = trail.length || 1;
  const rows = [];
  let cursor = 0;
  if (trail.length) {
    trail.forEach((s, i) => {
      const sec = Math.max(0, Number(s.elapsed_seconds) || 0);
      rows.push({
        label: `第 ${i + 1} 次 · ${s.upstream || "-"}${s.model ? " · " + s.model : ""}`,
        sec,
        start: cursor,
        cls: s.ok ? "ok" : "bad",
        status: s.status,
        error: s.reason || s.error,
      });
      cursor += sec;
    });
    if (total > cursor + 0.02) {
      rows.push({ label: "排队 / 收尾", sec: total - cursor, start: cursor, cls: "other" });
    }
  } else if (first > 0 && first < total - 0.02) {
    rows.push({ label: "首字等待", sec: first, start: 0, cls: "wait" });
    rows.push({ label: "生成", sec: total - first, start: first, cls: "gen" });
  } else {
    rows.push({ label: h.ok ? "完成" : "失败", sec: total, start: 0, cls: h.ok ? "ok" : "bad" });
  }
  const metrics = [
    ["总耗时", fmtLat(total)],
    ["首字", first > 0 ? fmtLat(first) : "—"],
    ["生成速度", tps && tps.gen ? fmtTps(tps.gen) + " tok/s" : (tps ? fmtTps(tps.overall) + " tok/s" : "—")],
    ["尝试", `${attempts} 次`],
  ].map(([k, v]) => `<span class="wf-metric"><em>${esc(k)}</em><b>${esc(v)}</b></span>`).join("");
  const rowHtml = rows.map((r, i) => {
    const left = total ? Math.max(0, r.start / total * 100) : 0;
    const width = total ? Math.max(0.8, r.sec / total * 100) : 100;
    const tip = `${r.label} · ${fmtLat(r.sec)}${r.status != null ? " · HTTP " + r.status : ""}${r.error ? " · " + explainHttpError(r.error, r.status) : ""}`;
    return `<div class="wf-row"><span class="wf-name" title="${esc(tip)}"><i class="wf-dot ${r.cls}"></i>${esc(shortLabel(r.label, 26))}</span><span class="wf-track" title="${esc(tip)}"><i class="wf-fill ${r.cls}" style="left:${left.toFixed(2)}%;width:${width.toFixed(2)}%"></i></span><span class="wf-time">${fmtLat(r.sec)}</span><span class="wf-status ${r.cls}">${r.status != null ? "HTTP " + r.status : (r.error ? "失败" : "完成")}</span></div>`;
  }).join("");
  return `<div class="wf"><div class="wf-head">${metrics}</div><div class="wf-rows">${rowHtml}</div><div class="wf-axis"><span>0</span><span>总耗时 ${fmtLat(total)}</span></div></div>`;
}

function histogramChart(host, bins) {
  if (!host) return;
  if (!bins || !bins.length) { chartEmpty(host); return; }
  const W = 640, H = 190, pad = { l: 40, r: 8, t: 20, b: 26 };
  const iw = W - pad.l - pad.r, ih = H - pad.t - pad.b;
  const key = bins.map(b => String(b.label)).join("\u0001");
  let svg = host.querySelector("svg.chart-svg");
  // 结构不变时只更新每根柱子的几何属性，让浏览器做过渡，避免每次刷新整块重绘闪一下。
  if (!svg || host._histKey !== key) {
    host._histKey = key;
    let grid = "";
    for (let g = 0; g <= 4; g++) {
      const gy = pad.t + (ih * g) / 4;
      grid += `<line x1="${pad.l}" x2="${W - pad.r}" y1="${gy}" y2="${gy}" stroke="rgba(255,255,255,.07)" stroke-width="1"/>`;
      grid += `<text class="hist-grid-label" data-g="${g}" x="${pad.l - 6}" y="${gy + 3}" font-size="9" fill="#8b9bb0" text-anchor="end"></text>`;
    }
    const bw0 = iw / bins.length;
    let body = "";
    bins.forEach((b, i) => {
      const x = pad.l + i * bw0 + bw0 * 0.18;
      const w = bw0 * 0.64;
      body += `<rect class="chart-vbar hist-bar" data-i="${i}" x="${x.toFixed(1)}" y="${(pad.t + ih).toFixed(1)}" width="${w.toFixed(1)}" height="0.5" rx="3" fill="rgba(255,255,255,.12)"><title></title></rect>`;
      body += `<text class="hist-count" data-i="${i}" font-size="9" fill="#8b9bb0" text-anchor="middle"></text>`;
      body += `<text x="${(x + w / 2).toFixed(1)}" y="${H - 6}" font-size="8.5" fill="#8b9bb0" text-anchor="middle">${esc(b.label)}</text>`;
    });
    host.innerHTML = `<svg class="chart-svg" viewBox="0 0 ${W} ${H}" role="img">${grid}${body}</svg>`;
    svg = host.querySelector("svg");
  }
  const max = chartMax(bins.map(b => b.count));
  svg.querySelectorAll(".hist-grid-label").forEach((el, g) => {
    el.textContent = chartAxis(max - (max * g) / 4);
  });
  const bw = iw / bins.length;
  bins.forEach((b, i) => {
    const v = Number(b.count) || 0;
    const h = (v / max) * ih;
    const x = pad.l + i * bw + bw * 0.18;
    const w = bw * 0.64;
    const y = pad.t + ih - h;
    const rect = svg.querySelector(`rect.hist-bar[data-i="${i}"]`);
    if (!rect) return;
    rect.setAttribute("x", x.toFixed(1));
    rect.setAttribute("y", Math.max(0, y).toFixed(1));
    rect.setAttribute("width", w.toFixed(1));
    rect.setAttribute("height", Math.max(0.5, h).toFixed(1));
    rect.setAttribute("fill", v ? CHART_COLORS.cyan : "rgba(255,255,255,.12)");
    let title = rect.querySelector("title");
    if (!title) { title = document.createElementNS("http://www.w3.org/2000/svg", "title"); rect.appendChild(title); }
    title.textContent = `${b.label}: ${fmtCount(v)}`;
    const cnt = svg.querySelector(`.hist-count[data-i="${i}"]`);
    if (cnt) {
      cnt.setAttribute("x", (x + w / 2).toFixed(1));
      cnt.setAttribute("y", Math.max(8, y - 4).toFixed(1));
      cnt.textContent = v ? chartAxis(v) : "";
    }
  });
}

function barChart(host, rows, opts = {}) {
  if (!host) return;
  if (!rows || !rows.length) { chartEmpty(host, opts.empty || "没有数据"); return; }
  const W = 640, rowH = opts.rowH || 22, labelW = opts.labelW || 138, valueW = 62;
  const H = rows.length * rowH + 4;
  const key = `${rows.length}|${rowH}|${labelW}|${opts.labelChars || 18}`;
  let svg = host.querySelector("svg.chart-svg");
  if (!svg || host._barKey !== key) {
    host._barKey = key;
    let body = "";
    rows.forEach((r, i) => {
      const cy = 4 + i * rowH + rowH / 2;
      body += `<text class="chart-bar-label" data-i="${i}" x="${labelW - 8}" y="${cy + 3.5}" font-size="10.5" fill="#c7d3e2" text-anchor="end"></text>`;
      body += `<rect y="${cy - 6}" height="12" rx="3" class="chart-bar" data-i="${i}"><title></title></rect>`;
      body += `<text class="chart-bar-value" data-i="${i}" y="${cy + 3.5}" font-size="10" fill="#8b9bb0"></text>`;
    });
    host.innerHTML = `<svg class="chart-svg" viewBox="0 0 ${W} ${H}" role="img">${body}</svg>`;
    svg = host.querySelector("svg");
  }
  const max = opts.max || chartMax(rows.map(r => r.value));
  rows.forEach((r, i) => {
    const cy = 4 + i * rowH + rowH / 2;
    const bw = Math.max(0, (Number(r.value) || 0) / max * (W - labelW - valueW));
    const label = svg.querySelector(`.chart-bar-label[data-i="${i}"]`);
    if (label) label.textContent = shortLabel(r.label, opts.labelChars || 18);
    const rect = svg.querySelector(`.chart-bar[data-i="${i}"]`);
    if (rect) {
      rect.setAttribute("x", labelW);
      rect.setAttribute("y", cy - 6);
      rect.setAttribute("width", Math.max(1, bw).toFixed(1));
      rect.setAttribute("fill", r.color || CHART_COLORS.blue);
      let t = rect.querySelector("title");
      if (!t) { t = document.createElementNS("http://www.w3.org/2000/svg", "title"); rect.appendChild(t); }
      t.textContent = r.title || r.label;
    }
    const val = svg.querySelector(`.chart-bar-value[data-i="${i}"]`);
    if (val) {
      val.setAttribute("x", (labelW + Math.max(1, bw) + 6).toFixed(1));
      val.textContent = r.display == null ? chartAxis(r.value) : r.display;
    }
  });
}

function radarChart(host, axes, series) {
  if (!host) return;
  const n = axes.length;
  if (!n || !series.length) { chartEmpty(host); return; }
  const W = 640, H = 300, cx = 320, cy = 150, R = 96;
  const key = `${axes.join("\u0001")}|${series.map(s => `${s.name}\u0001${s.color}`).join("|")}`;
  const pt = (i, r) => {
    const a = -Math.PI / 2 + i * 2 * Math.PI / n;
    return [cx + Math.cos(a) * r, cy + Math.sin(a) * r];
  };
  let svg = host.querySelector("svg.chart-svg");
  if (!svg || host._radarKey !== key) {
    host._radarKey = key;
    let g = "";
    [0.25, 0.5, 0.75, 1].forEach(k => {
      const pts = axes.map((_, i) => pt(i, R * k).map(v => v.toFixed(1)).join(",")).join(" ");
      g += `<polygon points="${pts}" fill="none" stroke="rgba(255,255,255,.09)" stroke-width="1"/>`;
    });
    axes.forEach((ax, i) => {
      const [x, y] = pt(i, R);
      g += `<line x1="${cx}" y1="${cy}" x2="${x.toFixed(1)}" y2="${y.toFixed(1)}" stroke="rgba(255,255,255,.08)"/>`;
      const [lx, ly] = pt(i, R + 22);
      g += `<text x="${lx.toFixed(1)}" y="${(ly + 4).toFixed(1)}" font-size="10.5" fill="#8b9bb0" text-anchor="middle">${esc(ax)}</text>`;
    });
    series.forEach((s, si) => {
      g += `<polygon fill="${s.color}" fill-opacity="${si ? 0.07 : 0.16}" stroke="${s.color}" stroke-width="1.8" class="radar-poly" data-s="${si}"/>`;
      s.values.forEach((_, i) => {
        g += `<circle r="2.5" fill="${s.color}" class="radar-dot" data-s="${si}" data-i="${i}"><title></title></circle>`;
      });
    });
    host.innerHTML = chartLegend(series) + `<svg class="chart-svg" viewBox="0 0 ${W} ${H}" role="img">${g}</svg>`;
    svg = host.querySelector("svg");
  }
  series.forEach((s, si) => {
    const pts = s.values.map((v, i) => pt(i, Math.max(2, R * Math.max(0, Math.min(100, Number(v) || 0)) / 100)).map(x => x.toFixed(1)).join(",")).join(" ");
    const poly = svg.querySelector(`.radar-poly[data-s="${si}"]`);
    if (poly) poly.setAttribute("points", pts);
    s.values.forEach((v, i) => {
      const [px, py] = pt(i, R * Math.max(0, Math.min(100, Number(v) || 0)) / 100);
      const c = svg.querySelector(`.radar-dot[data-s="${si}"][data-i="${i}"]`);
      if (!c) return;
      c.setAttribute("cx", px.toFixed(1));
      c.setAttribute("cy", py.toFixed(1));
      let t = c.querySelector("title");
      if (!t) { t = document.createElementNS("http://www.w3.org/2000/svg", "title"); c.appendChild(t); }
      t.textContent = `${axes[i]}: ${Math.round(v)}`;
    });
  });
}

function reqRangeLabel(r) {
  return r === "today" ? "今天 00:00 起" : r === "all" ? "全部（尾部缓存）" : String(r || "");
}
async function loadReqCharts(force) {
  const host = $("#reqTrendChart");
  if (!host || reqChartBusy) return;
  if (!force && Date.now() - reqChartAt < 20000) return;
  reqChartBusy = true;
  try {
    const range = $("#reqChartRange") ? $("#reqChartRange").value : "today";
    const qs = range === "today" ? "range=today" : (range === "all" || range === "0" ? "range=all" : "hours=" + encodeURIComponent(range));
    const d = await api("/stats/series?" + qs);
    reqChartAt = Date.now();
    renderReqCharts(d);
  } catch (e) {
    chartEmpty($("#reqTrendChart"), "图表加载失败：" + (e.message || e));
  } finally {
    reqChartBusy = false;
  }
}
function renderReqCharts(d) {
  const t = d.totals || {};
  let buckets = d.buckets || [];
  const firstIdx = buckets.findIndex(b => (b.calls || 0) > 0);
  if (firstIdx > 2) buckets = buckets.slice(firstIdx - 2);
  $("#reqChartMeta").textContent = `窗口 ${reqRangeLabel(d.range)} · ${fmtCount(t.calls)} 次调用 · 成功率 ${t.success_rate == null ? "—" : t.success_rate + "%"} · P50 ${chartMs(t.p50_ms)} · 首字 P50 ${chartMs(t.first_p50_ms)}`;
  lineChart($("#reqTrendChart"), buckets.map(b => b.label), [
    { name: "调用", values: buckets.map(b => b.calls || 0), color: CHART_COLORS.blue },
    { name: "失败", values: buckets.map(b => b.failed || 0), color: CHART_COLORS.red },
  ]);
  histogramChart($("#reqHistChart"), d.histogram || []);
  const models = (d.models || []).slice(0, 8).map(m => ({
    label: m.model,
    value: m.calls || 0,
    display: fmtCount(m.calls),
    title: `${m.model} · ${fmtCount(m.calls)} 次 · 输入 ${fmtNum(m.input)} / 输出 ${fmtNum(m.output)} · 平均 ${chartMs(m.avg_ms)}`,
    color: CHART_COLORS.blue,
  }));
  barChart($("#reqModelChart"), models, { labelChars: 28, labelW: 210, rowH: 22 });
}
if ($("#reqChartRange")) $("#reqChartRange").onchange = () => loadReqCharts(true);
if ($("#reqChartRefresh")) $("#reqChartRefresh").onclick = () => loadReqCharts(true);

function healthSpeedScore(sec) {
  const s = Number(sec);
  if (!Number.isFinite(s) || s <= 0) return 0;
  const lo = 0.3, hi = 20;
  const v = (Math.log10(hi) - Math.log10(Math.max(lo, Math.min(hi, s)))) / (Math.log10(hi) - Math.log10(lo));
  return Math.max(0, Math.min(100, v * 100));
}
function healthFirstScore(sec) {
  const s = Number(sec);
  if (!Number.isFinite(s) || s <= 0) return 0;
  const lo = 0.2, hi = 10;
  const v = (Math.log10(hi) - Math.log10(Math.max(lo, Math.min(hi, s)))) / (Math.log10(hi) - Math.log10(lo));
  return Math.max(0, Math.min(100, v * 100));
}
function healthAxes(r) {
  const p50 = Number(r.p50_seconds) || 0;
  const p95 = Number(r.p95_seconds) || 0;
  const stable = p50 > 0 && p95 > 0 ? Math.max(0, Math.min(100, 100 - (p95 / p50 - 1) * 28)) : (p50 > 0 ? 55 : 0);
  const input = Number(r.input) || 0, cr = Number(r.cache_read) || 0, cw = Number(r.cache_write) || 0;
  const cache = cr + cw + input > 0 ? Math.max(0, Math.min(100, cr / (cr + cw + input) * 100)) : 0;
  return [Number(r.success_rate) || 0, healthSpeedScore(r.p50_seconds), healthFirstScore(r.first_chunk_p50_seconds), stable, cache];
}
function renderHealthCharts(d) {
  const rows = (d.upstreams || []).filter(r => !r.external && (r.calls || 0) > 0);
  const barRows = rows.slice(0, 10).map(r => {
    const rate = Number(r.success_rate) || 0;
    return {
      label: r.upstream || "(未命名)",
      value: rate,
      display: rate.toFixed(1) + "%",
      title: `${r.upstream} · ${fmtCount(r.calls)} 次尝试 · 成功 ${fmtCount(r.ok)} / 失败 ${fmtCount(r.failed)} · P50 ${chartMs((r.p50_seconds || 0) * 1000)}`,
      color: rate >= 95 ? CHART_COLORS.green : rate >= 80 ? CHART_COLORS.warn : CHART_COLORS.red,
    };
  });
  barChart($("#healthBarChart"), barRows, { max: 100, labelChars: 16, empty: "这一窗口没有上游调用" });

  const sel = $("#healthRadarUp");
  if (!sel) return;
  if (!rows.length) {
    sel.innerHTML = "";
    chartEmpty($("#healthRadarChart"), "这一窗口没有上游调用");
    return;
  }
  const prev = sel.value;
  sel.innerHTML = rows.map(r => `<option value="${esc(r.upstream || "")}">${esc(r.upstream || "(未命名)")}</option>`).join("");
  if (prev && rows.some(r => (r.upstream || "") === prev)) sel.value = prev;
  const chosen = rows.find(r => (r.upstream || "") === sel.value) || rows[0];
  const axes = ["成功率", "响应速度", "首字速度", "稳定性", "缓存复用"];
  const avg = [0, 0, 0, 0, 0];
  rows.forEach(r => healthAxes(r).forEach((v, i) => { avg[i] += v / rows.length; }));
  radarChart($("#healthRadarChart"), axes, [
    { name: chosen.upstream || "(未命名)", values: healthAxes(chosen), color: CHART_COLORS.blue },
    { name: "全部平均", values: avg, color: CHART_COLORS.purple },
  ]);
}
if ($("#healthRadarUp")) $("#healthRadarUp").onchange = () => { if (healthData) renderHealthCharts(healthData); };

let sessTools = null;
let sessToolsSig = "";
let toolsRenderSig = "";
let toolsBusy = false;
function renderTools() {
  const meta = $("#sessToolMeta");
  if (!meta) return;
  if (!sessTools) { meta.textContent = "工具统计加载失败"; return; }
  const t = sessTools;
  const tot = t.totals || {};
  const rows = t.rows || [];
  meta.textContent = `窗口 ${reqRangeLabel(t.range)} · ${fmtCount(tot.calls)} 次工具调用 · ${fmtCount(tot.tools)} 个工具 · 成功率 ${tot.success_rate == null ? "—" : tot.success_rate + "%"} · 平均耗时 ${chartMs(tot.avg_ms)}`;
  setTabNum("tools", tot.calls || 0, false, "工具调用次数");
  const sig = JSON.stringify(rows.slice(0, 12).map(r => [r.name, r.calls, r.failed, r.avg_ms]));
  const tableSig = JSON.stringify([tot, rows]);
  if (tableSig === toolsRenderSig) return;
  toolsRenderSig = tableSig;
  if (sig !== sessToolsSig) {
    sessToolsSig = sig;
    barChart($("#sessToolChart"), rows.slice(0, 10).map(r => ({
      label: r.name,
      value: r.calls || 0,
      display: fmtCount(r.calls),
      title: `${r.name} · ${fmtCount(r.calls)} 次 · 失败 ${fmtCount(r.failed)} · 平均 ${chartMs(r.avg_ms)}`,
      color: (r.failed || 0) > 0 ? CHART_COLORS.warn : CHART_COLORS.cyan,
    })), { labelChars: 22, empty: "这一窗口没有工具调用" });
    const byMs = rows.filter(r => (r.avg_ms || 0) > 0).slice().sort((a, b) => (b.avg_ms || 0) - (a.avg_ms || 0)).slice(0, 10);
    barChart($("#sessToolChart2"), byMs.map(r => ({
      label: r.name,
      value: r.avg_ms || 0,
      display: chartMs(r.avg_ms),
      title: `${r.name} · 平均 ${chartMs(r.avg_ms)} · P50 ${chartMs(r.p50_ms)} / P95 ${chartMs(r.p95_ms)}`,
      color: CHART_COLORS.purple,
    })), { labelChars: 22, empty: "没有耗时数据" });
  }
  $("#sessToolRows").innerHTML = rows.length ? rows.map(r => {
    const src = Object.entries(r.by_source || {}).map(([k, v]) => `<span class="badge">${k === "dsh" ? "DSH" : k === "claude" ? "Claude" : esc(k)} ${fmtCount(v)}</span>`).join(" ");
    const errs = (r.errors || []).map(e => `${esc(e.message)}${e.count > 1 ? ` ×${e.count}` : ""}`).join("<br>");
    return `<tr>
      <td style="color:var(--fg)">${esc(r.name)}</td>
      <td>${src || "—"}</td>
      <td>${fmtCount(r.calls)}</td>
      <td style="color:${r.failed ? "var(--bad)" : "var(--dim)"}">${fmtCount(r.failed)}</td>
      <td>${r.success_rate == null ? "—" : r.success_rate + "%"}</td>
      <td>${fmtCount(r.sessions)}</td>
      <td>${chartMs(r.avg_ms)}</td>
      <td>${chartMs(r.p50_ms)}</td>
      <td>${chartMs(r.p95_ms)}</td>
      <td style="color:var(--dim)">${sessAt(r.last_at)}</td>
      <td style="color:var(--dim);max-width:280px;white-space:normal">${errs || "—"}</td>
    </tr>`;
  }).join("") : `<tr><td colspan="11" class="hint">这一窗口没有读到工具调用记录。</td></tr>`;
}

/* ================= 健康看板：按上游聚合成功率 / 分位延迟 / 冷却 ================= */
let healthData = null;
let healthBusy = false;
let healthOpen = new Set();

function healthCell(label, value, color) {
  return `<div class="tkn"><b${color ? ` style="color:${color}"` : ""}>${value == null ? "—" : value}</b><span>${label}</span></div>`;
}
function healthLat(v) { return v == null ? "—" : fmtLat(v); }
function healthPct(v) { return v == null ? "—" : v.toFixed(1) + "%"; }
function healthAt(v) {
  if (v == null) return "—";
  const d = new Date(v * 1000);
  return Number.isNaN(d.getTime()) ? "—" : d.toLocaleString("zh-CN", { hour12: false });
}

function renderHealth() {
  if (!healthData) return;
  const d = healthData;
  const t = d.totals || {};
  $("#healthThreshold").textContent = d.threshold ?? 3;
  $("#healthCooldown").textContent = d.cooldown_seconds ?? 60;
  const hr = ($("#healthRoute") && $("#healthRoute").value) || "all";
  const routeLabel = hr === "routed" ? " · 只看过 LLM Router" : hr === "external" ? " · 只看未走 LLM Router" : "";
  const extLabel = hr === "routed" && d.external_calls ? ` · 外部 Agent ${fmtCount(d.external_calls)} 轮` : "";
  const scanLabel = hr === "external"
    ? `外部会话 ${fmtCount(d.external_calls)} 轮`
    : hr === "all"
      ? `本机路由 ${fmtCount(d.kept)} 条 · 外部 Agent ${fmtCount(d.external_calls)} 轮`
      : `扫描 ${fmtCount(d.scanned)} 条 · 采用 ${fmtCount(d.kept)} 条`;
  $("#healthMeta").textContent = `窗口 ${d.range === "today" ? "今天 00:00 起" : d.range === "all" || d.hours === 0 ? "全部" : d.hours + "h"} · ${scanLabel}${routeLabel}${extLabel} · ${d.generated_at || ""}`;
  renderHealthCharts(d);
  $("#healthTotals").innerHTML = `<div class="tkn">
    ${healthCell(hr === "external" ? "外部轮次" : (hr === "routed" ? "路由尝试" : "调用尝试"), fmtCount(t.calls))}
    ${healthCell("成功", fmtCount(t.ok), "var(--ok)")}
    ${healthCell("失败", fmtCount(t.failed), t.failed ? "var(--bad)" : "")}
    ${healthCell(hr === "routed" ? "路由成功率" : "成功率", healthPct(t.success_rate), "var(--accent)")}
    ${healthCell("首字 P50", healthLat(t.first_chunk_p50_seconds))}
    ${healthCell("缓存读", fmtNum(t.cache_read))}
    ${healthCell("缓存写", fmtNum(t.cache_write))}
    ${healthCell("输入", fmtNum(t.input))}
    ${healthCell("输出", fmtNum(t.output))}
    ${d.external_calls && hr !== "external" ? healthCell("外部 Agent", fmtCount(d.external_calls), "var(--dim)") : ""}
  </div>`;

  const onlyBad = $("#healthOnlyBad").value === "1";
  let rows = (d.upstreams || []).slice();
  const bad = r => (r.failed || 0) > 0 || r.cooldown_remaining_seconds > 0 || r.consecutive_failures > 0 || r.available === false || r.enabled === false || r.orphan;
  if (hr === "all" && !(d.kept > 0)) rows = rows.filter(r => (r.attempts || r.calls || 0) > 0 || bad(r));
  if (onlyBad) rows = rows.filter(bad);
  const badCount = (d.upstreams || []).filter(bad).length;
  setTabNum("health", badCount, badCount > 0, "异常 / 冷却中的上游数");

  if (!rows.length) {
    $("#healthList").innerHTML = `<div class="hint">当前筛选下没有上游记录。</div>`;
    return;
  }
  $("#healthList").innerHTML = rows.map((r, i) => {
    const key = r.id || r.upstream || ("row" + i);
    const open = healthOpen.has(key);
    const statusBadge = r.external
      ? `<span class="badge">外部 Agent · 无延迟样本</span>`
      : r.orphan
      ? `<span class="badge">配置已删除</span>`
      : r.enabled === false
        ? `<span class="badge">已禁用</span>`
        : r.cooldown_remaining_seconds > 0
          ? `<span class="badge" style="color:var(--bad)">冷却中 ${r.cooldown_remaining_seconds}s</span>`
          : r.available === false
            ? `<span class="badge" style="color:var(--bad)">熔断中</span>`
            : `<span class="badge" style="color:var(--ok)">正常</span>`;
    const models = r.models || [];
    const modelRows = models.slice().sort((a, b) => (b.calls || 0) - (a.calls || 0)).map(m => `<tr>
      <td class="mono">${esc(m.model || "-")}</td>
      <td>${fmtCount(m.calls)}</td>
      <td style="color:var(--ok)">${fmtCount(m.ok)}</td>
      <td style="color:${m.failed ? "var(--bad)" : "var(--dim)"}">${fmtCount(m.failed)}</td>
      <td>${healthPct(m.success_rate)}</td>
      <td>${healthLat(m.p50_seconds)}</td>
      <td>${healthLat(m.p95_seconds)}</td>
      <td>${healthLat(m.first_chunk_p50_seconds)}</td>
      <td>${fmtNum(m.cache_read)}</td>
      <td>${fmtNum(m.cache_write)}</td>
    </tr>`).join("");
    return `<div class="card" style="margin-top:10px;padding:12px">
      <div class="row" style="flex-wrap:wrap;gap:8px;align-items:center">
        <b>${esc(r.upstream || "(未命名上游)")}</b>
        ${statusBadge}
        ${r.base_url ? `<span class="hint mono">${esc(r.base_url)}</span>` : ""}
        <div class="grow"></div>
        <button class="mini" data-health-act="clear" data-id="${esc(r.id || "")}" ${r.id ? "" : "disabled"} title="清掉这个上游的连续失败计数与冷却">清除熔断</button>
        ${r.external ? "" : `<button class="mini" data-health-act="toggle" data-key="${esc(key)}" title="把这个上游的统计按模型拆开，逐个模型看调用次数、成功率和耗时">${open ? "收起模型明细" : "展开模型明细 (" + models.length + ")"}</button>`}
      </div>
      <div class="tkn" style="margin-top:8px">
        ${healthCell("尝试", fmtCount(r.calls))}
        ${healthCell("成功", fmtCount(r.ok), "var(--ok)")}
        ${healthCell("失败", fmtCount(r.failed), r.failed ? "var(--bad)" : "")}
        ${healthCell("成功率", healthPct(r.success_rate), "var(--accent)")}
        ${healthCell("P50", healthLat(r.p50_seconds))}
        ${healthCell("P95", healthLat(r.p95_seconds))}
        ${healthCell("P99", healthLat(r.p99_seconds))}
        ${healthCell("首字 P50", healthLat(r.first_chunk_p50_seconds))}
        ${healthCell("缓存读", fmtNum(r.cache_read))}
        ${healthCell("缓存写", fmtNum(r.cache_write))}
        ${healthCell("连续失败", fmtCount(r.consecutive_failures))}
        ${healthCell("最近调用", healthAt(r.last_at))}
      </div>
      ${r.last_error || r.last_error_runtime ? `<div class="hint" style="margin-top:6px;color:var(--bad)">最后错误：${esc(r.last_error_runtime || r.last_error)}</div>` : ""}
      ${open ? `<div style="margin-top:10px;overflow-x:auto">
        <table class="mono" style="width:100%;font-size:12px;border-collapse:collapse">
          <thead><tr style="text-align:left;color:var(--dim)">
            <th>模型</th><th>尝试</th><th>成功</th><th>失败</th><th>成功率</th><th>P50</th><th>P95</th><th>首字 P50</th><th>缓存读</th><th>缓存写</th>
          </tr></thead>
          <tbody>${modelRows || `<tr><td colspan="10" class="hint">该窗口没有按模型的记录</td></tr>`}</tbody>
        </table>
      </div>` : ""}
    </div>`;
  }).join("");
}

async function loadHealth() {
  if (healthBusy) return;
  healthBusy = true;
  try {
    const hours = $("#healthHours") ? $("#healthHours").value : "today";
    const route = ($("#healthRoute") && $("#healthRoute").value) || "all";
    let qs = hours === "today" ? "range=today" : "hours=" + encodeURIComponent(hours);
    if (route !== "all") qs += "&route=" + encodeURIComponent(route);
    const d = await api("/health/upstreams?" + qs);
    healthData = d;
    renderHealth();
  } catch (e) {
    $("#healthList").innerHTML = `<div class="hint" style="color:var(--bad)">健康数据加载失败：${esc(e.message || e)}</div>`;
  } finally {
    healthBusy = false;
  }
}

async function clearHealth(id, all) {
  try {
    const r = await api("/health/upstreams/clear", all ? { all: true } : { id });
    toast(all ? "已清除全部熔断" : "已清除熔断", "ok");
    await loadHealth();
    return r;
  } catch (e) {
    toast("清除失败：" + (e.message || e), "bad");
  }
}

$("#healthRefresh").onclick = () => loadHealth();
if ($("#healthRoute")) $("#healthRoute").onchange = () => loadHealth();
$("#healthHours").onchange = () => loadHealth();
$("#healthOnlyBad").onchange = () => renderHealth();
$("#healthClearAll").onclick = async () => {
  if (!confirm("清除所有上游的连续失败计数与冷却，让被熔断的上游立刻重新参与路由？")) return;
  await clearHealth("", true);
};
$("#healthList").addEventListener("click", e => {
  const btn = e.target.closest("[data-health-act]");
  if (!btn) return;
  if (btn.dataset.healthAct === "toggle") {
    const k = btn.dataset.key;
    if (healthOpen.has(k)) healthOpen.delete(k); else healthOpen.add(k);
    renderHealth();
  } else if (btn.dataset.healthAct === "clear") {
    if (btn.dataset.id) clearHealth(btn.dataset.id, false);
  }
});
if (!AGENT_LOCAL) {
  const hb = document.querySelector('#tabs .tab[data-tab="health"]');
  if (hb) hb.style.display = "none";
}

/* ================= 会话对账：读各家 Agent 会话记录，找没走本机路由的调用 ================= */
let sessData = null;
let sessBusy = false;
let sessRenderSig = "";

function sessAt(ms) {
  if (!ms) return "—";
  const d = new Date(ms);
  return Number.isNaN(d.getTime()) ? "—" : d.toLocaleString("zh-CN", { hour12: false });
}
function sessSourceMeta(source) {
  const map = {
    dsh: { label: "DeepSeek Harness", letter: "D", hue: 214 },
    claude: { label: "Claude Code", letter: "C", hue: 276 },
    pi: { label: "Pi", letter: "P", hue: 146 },
    codex: { label: "Codex", letter: "X", hue: 28 },
  };
  return map[source] || { label: source || "未知", letter: String(source || "?").slice(0, 1).toUpperCase(), hue: 210 };
}
function sessDuration(ms) {
  if (!ms || ms < 0) return "—";
  const sec = Math.round(ms / 1000);
  if (sec < 60) return sec + "s";
  const min = Math.round(sec / 60);
  if (min < 60) return min + "m";
  const h = Math.floor(min / 60), m = min % 60;
  return h + "h" + (m ? " " + m + "m" : "");
}
function sessStat(label, value, sub, tone, icon) {
  return `<div class="sess-stat ${tone || ""}">
    <span class="sess-stat-icon">${icon || "•"}</span>
    <div class="sess-stat-body"><b>${value == null ? "—" : value}</b><span>${label}</span>${sub ? `<small>${sub}</small>` : ""}</div>
  </div>`;
}
function sessSources() {
  const ids = [];
  if ($("#sessSrcDsh") && $("#sessSrcDsh").checked) ids.push("dsh");
  if ($("#sessSrcClaude") && $("#sessSrcClaude").checked) ids.push("claude");
  if ($("#sessSrcPi") && $("#sessSrcPi").checked) ids.push("pi");
  if ($("#sessSrcCodex") && $("#sessSrcCodex").checked) ids.push("codex");
  return ids;
}
function sessRow(r) {
  const meta = sessSourceMeta(r.source);
  const routed = r.routed
    ? `<span class="sess-route ok"><i></i>走路由</span>`
    : `<span class="sess-route bad"><i></i>未走路由</span>`;
  return `<tr>
    <td><span class="sess-src"><span class="sess-src-dot" style="--h:${meta.hue}">${meta.letter}</span><b title="${esc(r.agent || meta.label)}">${esc(r.agent || meta.label)}</b></span></td>
    <td><code class="sess-code provider" title="${esc(r.provider || "-")}">${esc(r.provider || "-")}</code></td>
    <td><code class="sess-code" title="${esc(r.model || "-")}">${esc(r.model || "-")}</code></td>
    <td><span class="sess-url" title="${esc(r.base_url || "")}">${r.base_url ? esc(r.base_url) : `<span class="hint">未知</span>`}</span></td>
    <td>${routed}</td>
    <td class="num">${fmtCount(r.rounds)}</td>
    <td class="num">${fmtNum(r.input)}</td>
    <td class="num">${fmtNum(r.output)}</td>
    <td class="num">${fmtNum(r.cache_read)}</td>
    <td class="num">${fmtNum(r.cache_write)}</td>
    <td class="sess-timecell"><span title="首次 ${esc(sessAt(r.first_ms))}">${sessAt(r.first_ms)}</span><span title="最近 ${esc(sessAt(r.last_ms))}">${sessAt(r.last_ms)}</span></td>
  </tr>`;
}

function sessItem(s) {
  const meta = sessSourceMeta(s.source);
  const total = s.rounds || 0, ext = s.external_rounds || 0, routed = s.routed_rounds || 0;
  const pct = total ? (routed / total) * 100 : 0;
  const dur = sessDuration((s.ended_ms || 0) - (s.started_ms || 0));
  const models = (s.external_models || []).slice(0, 6).map(m =>
    `<span class="sess-model-chip" title="${esc(m.label)} · ${fmtCount(m.rounds)} 轮">${esc(m.label)} · ${fmtCount(m.rounds)}</span>`
  ).join("");
  const paths = (s.cwd || s.path) ? `<div class="sess-path">
    ${s.cwd ? `<span title="${esc(s.cwd)}">${esc(s.cwd)}</span>` : ""}
    ${s.path ? `<span title="${esc(s.path)}">${esc(s.path)}</span>` : ""}
  </div>` : "";
  return `<div class="sess-item ${ext ? "bad" : "ok"}">
    <div class="sess-item-head">
      <span class="sess-src-dot" style="--h:${meta.hue}">${meta.letter}</span>
      <b>${esc(s.agent || meta.label)}</b>
      <span class="badge blue">${esc(s.project || "(未知项目)")}</span>
      ${ext ? `<span class="sess-route bad"><i></i>未走路由 ${fmtCount(ext)} 轮</span>` : `<span class="sess-route ok"><i></i>全部走路由</span>`}
      <span class="sess-item-time">最近 ${sessAt(s.ended_ms)}</span>
    </div>
    <div class="sess-item-stats">
      <div class="sess-item-stat"><b>${fmtCount(total)}</b><span>总轮数</span></div>
      <div class="sess-item-stat ok"><b>${pct.toFixed(1)}%</b><span>路由覆盖</span></div>
      <div class="sess-item-stat ${ext ? "bad" : ""}"><b>${fmtCount(ext)}</b><span>未走路由</span></div>
      <div class="sess-item-stat"><b>${dur}</b><span>会话时长</span></div>
    </div>
    ${models ? `<div class="sess-item-models"><span class="hint" style="font-size:9.5px">外部模型</span>${models}</div>` : ""}
    ${paths}
  </div>`;
}

function renderSessions() {
  if (!sessData) return;
  const d = sessData;
  const t = d.totals || {};
  const e = d.external_totals || {};
  const j = d.journal || {};
  const total = Number(t.rounds) || 0;
  const ext = Number(e.rounds) || 0;
  const routed = Number(j.rounds) || 0;
  const all = ext + routed;
  const coverage = all ? (routed / all) * 100 : 0;
  const extPct = all ? (ext / all) * 100 : 0;
  const sessRange = d.range === "today" ? "今天 00:00 起" : (d.range === "all" || d.window_hours <= 0 ? "全部" : d.window_hours + "h");
  const auto = sessRefreshSeconds();
  const autoHint = $("#sessAutoHint");
  if (autoHint) autoHint.textContent = String(auto);
  const routeMode = ($("#sessRoute") && $("#sessRoute").value) || "all";
  const needle = ($("#sessSearch") && $("#sessSearch").value || "").trim().toLowerCase();
  const dataSig = JSON.stringify({ range: d.range, route: routeMode, totals: t, external: e, journal: j, rows: d.rows, sessions: d.sessions, sources: d.sources, needle });
  const metaHtml = [
    ["窗口", sessRange],
    ["Provider/模型", fmtCount(d.rows_total) + " 个"],
    ["会话", fmtCount(d.sessions_total) + " 个"],
    ["用时", (d.elapsed_ms || 0) + "ms"],
    ["扫描于", sessAt(d.generated_at)],
    ["自动刷新", auto + "s"],
  ].map(([k, v]) => `<span><i>${k}</i><b>${esc(v)}</b></span>`).join("");
  $("#sessMeta").innerHTML = metaHtml;
  if (dataSig === sessRenderSig) return;
  sessRenderSig = dataSig;

  const health = $("#sessHealth");
  if (health) {
    health.className = "sess-pill " + (ext ? "bad" : "ok");
    health.textContent = all ? `今日 ${fmtCount(all)} 轮 · 过路由 ${fmtCount(routed)} 轮` : "暂无记录";
  }

  $("#sessTotals").innerHTML = [
    sessStat("今日所有请求", fmtCount(all), "过路由 + 未走路由", "accent", "◷"),
    sessStat("过 LLM Router", fmtCount(routed), all ? `包含在 ${fmtCount(all)} 轮中 · ${coverage.toFixed(1)}%` : "—", "ok", "✓"),
    sessStat("未走路由", fmtCount(ext), all ? `占 ${extPct.toFixed(1)}%` : "—", ext ? "bad" : "ok", "⇄"),
    sessStat("输入 tokens", fmtNum(t.input), "会话记录累计", "", "↓"),
    sessStat("输出 tokens", fmtNum(t.output), "会话记录累计", "", "↑"),
    sessStat("缓存读", fmtNum(t.cache_read), "prompt cache hit", "", "◈"),
    sessStat("缓存写", fmtNum(t.cache_write), "prompt cache write", "", "▣"),
    sessStat("会话数", fmtCount(d.sessions_total || 0), "所有数据源", "", "▤"),
  ].join("");

  const jLine = j.rounds
    ? `本机 8010 流水 ${fmtCount(j.rounds)} 轮（输入 ${fmtNum(j.input)} / 输出 ${fmtNum(j.output)}）`
    : `本机 8010 流水这一窗口没有记录（${esc(j.path || "config.history.jsonl")}）`;
  const byModel = (j.by_model || []).slice(0, 5).map(m =>
    `<code>${esc(m.model)} · ${fmtCount(m.rounds)} 轮</code>`
  ).join("");
  $("#sessCompare").innerHTML = `
    <div class="sess-compare-top">
      <b>今日请求构成</b>
      <span class="hint">全部 ${fmtCount(all)} 轮 = 过 LLM Router ${fmtCount(routed)} 轮 + 未走路由 ${fmtCount(ext)} 轮 · ${jLine}</span>
      <div class="sess-compare-legend">
        <span><i class="ok"></i>走路由 ${fmtCount(routed)} 轮</span>
        <span><i class="bad"></i>未走路由 ${fmtCount(ext)} 轮</span>
      </div>
    </div>
    <div class="sess-ratio">
      <span class="ok" style="width:${coverage.toFixed(2)}%"></span>
      <span class="bad" style="width:${(100 - coverage).toFixed(2)}%"></span>
    </div>
    <div class="sess-compare-foot"><span><b>${coverage.toFixed(1)}%</b> 经过本机路由</span><span><b>${extPct.toFixed(1)}%</b> 绕过</span></div>
    ${byModel ? `<div class="sess-compare-note"><span>流水模型 Top</span>${byModel}</div>` : ""}`;

  const srcs = d.sources || [];
  $("#sessSources").innerHTML = srcs.length
    ? srcs.map(s => {
        const meta = sessSourceMeta(s.id);
        const active = (s.rounds || 0) > 0;
        const skipped = s.skipped_old ? ` · 跳过 ${fmtCount(s.skipped_old)} 个旧文件` : "";
        return `<div class="sess-source ${active ? "" : "off"}">
          <div class="sess-source-top"><span class="sess-src-dot" style="--h:${meta.hue}">${meta.letter}</span><b>${esc(s.name || meta.label)}</b><span class="sess-source-rounds">${fmtCount(s.rounds)} 轮</span></div>
          <div class="sess-source-meta">${fmtCount(s.files)} 文件 · ${fmtCount(s.sessions)} 会话 · ${fmtBytes(s.bytes || 0)} · ${fmtCount(s.ms || 0)}ms${s.truncated ? " · 只读尾部" : ""}${skipped}</div>
          <div class="sess-source-path" title="${esc(s.root || "")}">${esc(s.root || "")}${s.note ? ` · ${esc(s.note)}` : ""}</div>
        </div>`;
      }).join("")
    : `<div class="sess-empty">没有启用的数据源。</div>`;

  setTabNum("sessions", ext, ext > 0, "窗口内没走本机路由的调用轮数");

  let rows = (d.rows || []).slice();
  if (routeMode === "external") rows = rows.filter(r => !r.routed);
  else if (routeMode === "routed") rows = rows.filter(r => r.routed);
  if (needle) rows = rows.filter(r => [r.agent, r.source, r.provider, r.model, r.base_url].join(" ").toLowerCase().includes(needle));
  $("#sessRowsMeta").textContent = `${fmtCount(rows.length)} / ${fmtCount(d.rows_total || 0)} 行`;
  $("#sessRows").innerHTML = rows.length
    ? rows.map(sessRow).join("")
    : `<tr><td colspan="11" class="sess-empty">没有匹配的记录，试试换个筛选条件。</td></tr>`;

  let sess = (d.sessions || []).slice();
  if (routeMode === "external") sess = sess.filter(s => (s.external_rounds || 0) > 0);
  else if (routeMode === "routed") sess = sess.filter(s => (s.routed_rounds || 0) > 0);
  if (needle) sess = sess.filter(s => [s.agent, s.source, s.project, s.cwd, s.path].concat((s.external_models || []).map(m => m.label)).join(" ").toLowerCase().includes(needle));
  $("#sessListMeta").textContent = `${fmtCount(sess.length)} / ${fmtCount(d.sessions_total || 0)} 个`;
  $("#sessList").innerHTML = sess.length
    ? sess.map(sessItem).join("")
    : `<div class="sess-empty">这一筛选下没有会话记录。</div>`;
}

async function loadSessions() {
  if (sessBusy) return;
  sessBusy = true;
  const btn = $("#sessReload");
  if (btn) btn.classList.add("busy");
  const meta = $("#sessMeta");
  if (meta) meta.textContent = "正在扫描会话记录…";
  try {
    const win = $("#sessHours") ? $("#sessHours").value : "today";
    const src = sessSources();
    if (!src.length) {
      $("#sessRows").innerHTML = `<tr><td colspan="11" class="sess-empty">至少要勾一个数据源。</td></tr>`;
      $("#sessList").innerHTML = `<div class="sess-empty">至少要勾一个数据源。</div>`;
      $("#sessTotals").innerHTML = "";
      $("#sessCompare").innerHTML = "";
      $("#sessSources").innerHTML = "";
      return;
    }
    const qs = new URLSearchParams({ sources: src.join(","), limit: "200" });
    const route = ($("#sessRoute") && $("#sessRoute").value) || "all";
    if (route !== "all") qs.set("route", route);
    if (win === "today") qs.set("range", "today");
    else qs.set("hours", win);
    qs.set("metric", timingMode());
    const d = await api("/sessions/scan?" + qs.toString());
    sessData = d;
    renderSessions();
  } catch (e) {
    $("#sessRows").innerHTML = `<tr><td colspan="11" class="sess-empty" style="color:var(--bad)">会话扫描失败：${esc(e.message || e)}</td></tr>`;
    if (meta) meta.textContent = "会话扫描失败：" + (e.message || e);
  } finally {
    sessBusy = false;
    if (btn) btn.classList.remove("busy");
  }
}

/* ================= Prompt Cache 账本 ================= */
let cacheData = null;
let cacheBusy = false;

function cacheRowHtml(r, kind) {
  const cells = kind === "session"
    ? `<td class="mono">${esc(r.label)}</td>
       <td>${esc(r.model || "—")}</td>
       <td>${esc((r.upstreams || []).join(" / ") || "—")}</td>
       <td class="num">${fmtCount(r.calls)}</td>
       <td class="num">${fmtCount(r.cache_read)}</td>
       <td class="num">${Number(r.cache_write || 0) ? fmtCount(r.cache_write) : "—"}</td>
       <td class="num">${Number(r.hit_rate || 0).toFixed(1)}%</td>
       <td class="num">${fmtCount(r.switches)}</td>`
    : `<td>${esc(r.label)}</td>
       <td class="num">${fmtCount(r.calls)}</td>
       <td class="num">${fmtCount(r.input)}</td>
       <td class="num">${fmtCount(r.output)}</td>
       <td class="num">${fmtCount(r.cache_read)}</td>
       <td class="num">${Number(r.cache_write || 0) ? fmtCount(r.cache_write) : "—"}</td>
       <td class="num">${Number(r.hit_rate || 0).toFixed(1)}%</td>
       <td class="num">${fmtCount(r.saved_tokens)}</td>`;
  return `<tr>${cells}</tr>`;
}

function renderCacheLedger() {
  const d = cacheData;
  if (!d) return;
  const t = d.totals || {};
  const range = d.range === "today" ? "今天 00:00 起"
    : d.range === "all" ? "全部" : d.range;
  $("#cacheMeta").textContent = `${range} · 全部 Agent 会话 ${fmtCount(t.calls)} 轮 · 其中走本机路由 ${fmtCount(t.routed_calls)} 轮 · 缓存命中率 ${Number(t.hit_rate || 0).toFixed(1)}%`;
  $("#cacheStats").innerHTML = [
    sessStat("全部 Agent 会话", fmtCount(t.calls), `其中走本机路由 ${fmtCount(t.routed_calls)} 轮 · 粘性会话 ${fmtCount(d.affinity_sticks)}`, "accent", "◷"),
    sessStat("缓存读", fmtCount(t.cache_read), `其中路由请求 ${fmtCount(t.routed_cache_read || 0)} tokens`, "ok", "↙"),
    sessStat("缓存写", Number(t.cache_write || 0) ? fmtCount(t.cache_write) : "—", Number(t.cache_write || 0) ? `其中路由请求 ${fmtCount(t.routed_cache_write || 0)} tokens` : "Agent 会话记录未提供", "warn", "↗"),
    sessStat("缓存命中率", `${Number(t.hit_rate || 0).toFixed(1)}%`, `提示词 ${fmtCount(t.prompt_tokens)} tokens`, "", "◎"),
  ].join("");
  const modelRows = (d.models || []).slice(0, 8).map(r => ({
    label: r.label, value: r.cache_read, display: fmtCount(r.cache_read),
    title: `${r.label} · 缓存读 ${fmtCount(r.cache_read)} · 命中率 ${Number(r.hit_rate || 0).toFixed(1)}%`,
  }));
  barChart($("#cacheModelChart"), modelRows, { labelChars: 22, labelW: 150, empty: "还没有缓存读记录" });
  const upRows = (d.upstreams || []).slice(0, 8).map(r => ({
    label: r.label, value: r.cache_read, display: fmtCount(r.cache_read),
    color: CHART_COLORS.cyan,
    title: `${r.label} · 缓存读 ${fmtCount(r.cache_read)} · 命中率 ${Number(r.hit_rate || 0).toFixed(1)}%`,
  }));
  barChart($("#cacheUpChart"), upRows, { labelChars: 22, labelW: 150, empty: "还没有上游缓存记录" });

  const table = (title, rows, kind, head) => `<div class="card" style="margin-top:12px">
    <h3 style="margin:0 0 8px">${title}</h3>
    <div class="sess-table-wrap"><table class="sess-table">
      <thead><tr>${head}</tr></thead>
      <tbody>${rows.length ? rows.map(r => cacheRowHtml(r, kind)).join("") :
        `<tr><td colspan="${head.split("</th>").length - 1}" class="sess-empty">暂无记录</td></tr>`}</tbody>
    </table></div></div>`;
  const modelHead = "<th>模型</th><th>请求</th><th>输入</th><th>输出</th><th>缓存读</th><th>缓存写</th><th>命中率</th><th>节省 token</th>";
  const upHead = "<th>上游</th><th>请求</th><th>输入</th><th>输出</th><th>缓存读</th><th>缓存写</th><th>命中率</th><th>节省 token</th>";
  const sessHead = "<th>会话</th><th>模型</th><th>上游</th><th>请求</th><th>缓存读</th><th>缓存写</th><th>命中率</th><th>切换上游</th>";
  const cwNote = Number(t.cache_write || 0) ? "" :
    `<div class="hint" style="margin:10px 0 0">缓存写：当前 Agent 会话记录均未提供 cache_write 字段（DSH/Claude/Pi/Codex 多数不记录），所以显示 —，不是统计失败；本机路由流水里的缓存写请在「请求记录」查看。</div>`;
  $("#cacheTables").innerHTML = cwNote +
    table("按模型", d.models || [], "model", modelHead) +
    table("按上游", d.upstreams || [], "upstream", upHead) +
    table("按会话（最近活动，最多 100）", (d.sessions || []).slice(0, 100), "session", sessHead);
  setTabNum("cache", t.calls || 0, false, "缓存账本条数");
}

async function loadCacheLedger() {
  if (cacheBusy) return;
  cacheBusy = true;
  const btn = $("#cacheRefresh");
  if (btn) btn.classList.add("busy");
  const meta = $("#cacheMeta");
  if (meta) meta.textContent = "正在统计 Prompt Cache…";
  try {
    const range = $("#cacheRange") ? $("#cacheRange").value : "today";
    const route = ($("#cacheRoute") && $("#cacheRoute").value) || "all";
    const qs = new URLSearchParams();
    if (route !== "all") qs.set("route", route);
    if (range === "today") qs.set("range", "today");
    else if (range === "0") qs.set("range", "all");
    else qs.set("hours", range);
    cacheData = await api("/stats/cache-ledger?" + qs.toString());
    renderCacheLedger();
  } catch (e) {
    if (meta) meta.textContent = "缓存账本加载失败：" + (e.message || e);
  } finally {
    cacheBusy = false;
    if (btn) btn.classList.remove("busy");
  }
}

function toolSources() {
  const ids = [];
  if ($("#toolSrcDsh") && $("#toolSrcDsh").checked) ids.push("dsh");
  if ($("#toolSrcClaude") && $("#toolSrcClaude").checked) ids.push("claude");
  return ids;
}
async function loadTools() {
  if (toolsBusy) return;
  toolsBusy = true;
  const meta = $("#sessToolMeta");
  const win = $("#toolHours") ? $("#toolHours").value : "today";
  const src = toolSources();
  if (!src.length) {
    if (meta) meta.textContent = "至少要勾一个数据源。";
    toolsBusy = false;
    return;
  }
  if (meta) meta.textContent = "正在统计工具调用…";
  try {
    const qs = new URLSearchParams({ sources: src.join(","), limit: "200" });
    if (win === "today") qs.set("range", "today");
    else qs.set("hours", win);
    sessTools = await api("/sessions/tools?" + qs.toString());
    renderTools();
  } catch (e) {
    if (meta) meta.textContent = "工具统计加载失败：" + (e.message || e);
  } finally {
    toolsBusy = false;
  }
}
if ($("#toolReload")) $("#toolReload").onclick = () => loadTools();
if ($("#toolHours")) $("#toolHours").onchange = () => loadTools();
["toolSrcDsh", "toolSrcClaude"].forEach(id => {
  const el = document.getElementById(id);
  if (el) el.onchange = () => loadTools();
});

if ($("#cacheRefresh")) $("#cacheRefresh").onclick = () => loadCacheLedger();
if ($("#cacheRange")) $("#cacheRange").onchange = () => loadCacheLedger();
if ($("#cacheRoute")) $("#cacheRoute").onchange = () => loadCacheLedger();

if ($("#sessReload")) $("#sessReload").onclick = () => loadSessions();
if ($("#sessHours")) $("#sessHours").onchange = () => loadSessions();
["sessSrcDsh", "sessSrcClaude", "sessSrcPi", "sessSrcCodex"].forEach(id => {
  const el = document.getElementById(id);
  if (el) el.onchange = () => loadSessions();
});
if ($("#sessRoute")) $("#sessRoute").onchange = () => loadSessions();
if ($("#sessSearch")) $("#sessSearch").oninput = () => renderSessions();

// 自动刷新：跟随「设置 → 界面刷新 → 会话对账刷新间隔」（默认 5 秒）；只在本页可见且没有正在扫描时执行。
let sessTimer = null;
function applySessAuto() {
  if (sessTimer) { clearInterval(sessTimer); sessTimer = null; }
  if (!AGENT_LOCAL) return;
  const hint = $("#sessAutoHint");
  if (hint) hint.textContent = String(sessRefreshSeconds());
  const ms = Math.max(1000, sessRefreshSeconds() * 1000);
  sessTimer = setInterval(() => {
    if (document.hidden || sessBusy) return;
    const pane = document.getElementById("pane-sessions");
    if (!pane || pane.hidden) return;
    loadSessions();
  }, ms);
}
applySessAuto();

// MCP 工具页也按会话对账的间隔自动刷新（它同样只是只读统计文件）。
let toolsTimer = null;
function applyToolsAuto() {
  if (toolsTimer) { clearInterval(toolsTimer); toolsTimer = null; }
  if (!AGENT_LOCAL) return;
  const ms = Math.max(1000, sessRefreshSeconds() * 1000);
  toolsTimer = setInterval(() => {
    if (document.hidden || toolsBusy) return;
    const pane = document.getElementById("pane-tools");
    if (!pane || pane.hidden) return;
    loadTools();
  }, ms);
}
applyToolsAuto();
if (!AGENT_LOCAL) {
  ["sessions", "tools", "cache"].forEach(t => {
    const sb = document.querySelector('#tabs .tab[data-tab="' + t + '"]');
    if (sb) sb.style.display = "none";
  });
}

(async function boot() {
  serverUrl = location.origin;
  bindDelegatedEvents();
  // 记住上次看的是哪个 tab
  let saved = null;
  try { saved = localStorage.getItem("llm-router-tab"); } catch (e) {}
  setTab(saved || "req");
  await loadWindowChrome();
  syncTabsTop();
  await refresh();
  applyRefreshInterval();
  startSocket();
})();
