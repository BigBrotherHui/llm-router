<script>
  const LS_TOKEN = "llm-relay-token";

  let token = $state(new URLSearchParams(location.search).get("t") || localStorage.getItem(LS_TOKEN) || "");
  let connected = $state(false);
  let lastAt = $state(0);
  let totals = $state({});
  let upstreams = $state([]);
  let models = $state([]);
  let journal = $state([]);
  let journalTotal = $state(0);
  let tab = $state("overview");
  let ws = null;
  let retry = 0;

  // 先用普通 HTTP 把边缘缓存的快照拿回来（比等 WebSocket 握手快一截），
  // 之后再用 WS 收增量。
  async function pullSnapshot() {
    if (!token) return;
    try {
      const r = await fetch(`/snapshot?token=${encodeURIComponent(token)}`, { cache: "no-store" });
      const m = await r.json();
      if (m && m.type === "snapshot") applySnapshot(m);
    } catch (e) {}
  }

  const fmt = (n) => {
    const v = Number(n || 0);
    if (v >= 1e9) return (v / 1e9).toFixed(1) + "B";
    if (v >= 1e6) return (v / 1e6).toFixed(1) + "M";
    if (v >= 1e3) return (v / 1e3).toFixed(1) + "K";
    return String(v);
  };
  const pct = (a, b) => (b > 0 ? Math.round((a / b) * 100) : 0);

  function applySnapshot(m) {
    totals = m.totals || {};
    upstreams = m.upstreams || [];
    models = m.models || [];
    journal = m.journal || [];
    journalTotal = m.journal_total || journal.length;
    lastAt = m.at || Date.now() / 1000;
    try { localStorage.setItem("llm-relay-snapshot", JSON.stringify(m)); } catch (e) {}
  }

  // 上次看到的快照先渲染出来，页面立刻有内容，再去边缘取最新。
  function loadCached() {
    try {
      const raw = localStorage.getItem("llm-relay-snapshot");
      if (raw) applySnapshot(JSON.parse(raw));
    } catch (e) {}
  }

  function connect() {
    if (!token) return;
    const proto = location.protocol === "https:" ? "wss://" : "ws://";
    try {
      ws = new WebSocket(`${proto}${location.host}/client?token=${encodeURIComponent(token)}`);
    } catch (e) {
      return scheduleRetry();
    }
    ws.onopen = () => { connected = true; retry = 0; ws.send(JSON.stringify({ type: "pull" })); };    ws.onclose = () => { connected = false; scheduleRetry(); };
    ws.onerror = () => { try { ws.close(); } catch (e) {} };
    ws.onmessage = (ev) => {
      let m;
      try { m = JSON.parse(ev.data); } catch (e) { return; }
      if (m.type === "snapshot") applySnapshot(m);
      else if (m.type === "journal" && m.entry) {
        const key = String(m.entry.at);
        if (!journal.some((x) => String(x.at) === key)) {
          journal = [m.entry, ...journal].slice(0, 200);
          journalTotal += 1;
        }
      } else if (m.type === "state" && ws && ws.readyState === 1) {
        ws.send(JSON.stringify({ type: "pull" }));
      }
    };
  }

  function scheduleRetry() {
    retry = Math.min(retry + 1, 6);
    setTimeout(connect, Math.min(1000 * 2 ** retry, 15000));
  }

  function saveToken() {
    localStorage.setItem(LS_TOKEN, token);
    if (ws) { try { ws.close(); } catch (e) {} }
    connect();
  }

  $effect(() => { loadCached(); pullSnapshot(); connect(); });
</script>

<div class="wrap">
  <header>
    <div class="brand">llm-router <span class="dim">edge</span></div>
    <div class="dot" class:on={connected}></div>
    <div class="dim small">{connected ? "已连接" : "未连接"}</div>
  </header>

  {#if !token}
    <section class="card">
      <h2>输入中继令牌</h2>
      <input bind:value={token} placeholder="RELAY_TOKEN" />
      <button onclick={saveToken}>连接</button>
    </section>
  {:else}
    <nav>
      {#each [["overview", "概览"], ["up", "上游"], ["model", "模型"], ["req", "记录"]] as [k, label]}
        <button class:active={tab === k} onclick={() => (tab = k)}>{label}</button>
      {/each}
    </nav>

    {#if tab === "overview"}
      <section class="grid">
        <div class="metric"><div class="k">总请求</div><div class="v">{fmt(totals.calls)}</div></div>
        <div class="metric"><div class="k">成功</div><div class="v ok">{fmt(totals.ok)}</div></div>
        <div class="metric"><div class="k">失败</div><div class="v bad">{fmt(totals.failed)}</div></div>
        <div class="metric"><div class="k">上游尝试</div><div class="v">{fmt(totals.attempts)}</div></div>
        <div class="metric"><div class="k">输入 token</div><div class="v">{fmt(totals.input)}</div></div>
        <div class="metric"><div class="k">输出 token</div><div class="v">{fmt(totals.output)}</div></div>
      </section>
      <section class="card">
        <h2>最近请求</h2>
        {#each journal.slice(0, 15) as e}
          <div class="row">
            <span class="mono">{e.time || ""}</span>
            <span class="badge" class:bad={!e.ok}>{e.status || "-"}</span>
            <span class="grow">{e.model || ""}</span>
            <span class="dim small">{e.upstream || ""}</span>
          </div>
        {/each}
      </section>
    {:else if tab === "up"}
      <section class="card">
        <h2>上游 {upstreams.length}</h2>
        {#each upstreams as u}
          <div class="row">
            <span class="dot" class:on={u.enabled}></span>
            <span class="grow">{u.name}</span>
            <span class="dim small">{u.selected}/{u.available}</span>
            {#if u.cooling}<span class="badge bad">冷却</span>{/if}
          </div>
        {/each}
      </section>
    {:else if tab === "model"}
      <section class="card">
        <h2>模型路由 {models.length}</h2>
        {#each models as m}
          <div class="row">
            <span class="grow mono">{m.name}</span>
            <span class="dim small">{m.enabled}/{m.entries} 条</span>
          </div>
        {/each}
      </section>
    {:else}
      <section class="card">
        <h2>请求记录 {fmt(journalTotal)}</h2>
        {#each journal as e}
          <div class="row">
            <span class="mono">{e.time || ""}</span>
            <span class="badge" class:bad={!e.ok}>{e.status || "-"}</span>
            <span class="grow">{e.model || ""}</span>
            <span class="dim small">{e.upstream || ""}</span>
            <span class="dim small">{e.elapsed_seconds != null ? e.elapsed_seconds.toFixed(2) + "s" : ""}</span>
          </div>
        {/each}
      </section>
    {/if}
  {/if}
</div>

<style>
  :global(body) { margin: 0; background: #0b0f17; color: #e6edf3; font: 14px/1.5 system-ui, -apple-system, "PingFang SC", sans-serif; }
  .wrap { max-width: 720px; margin: 0 auto; padding: 12px 12px 40px; }
  header { display: flex; align-items: center; gap: 8px; padding: 8px 4px 12px; }
  .brand { font-weight: 600; }
  .dim { color: #8b98a8; }
  .small { font-size: 12px; }
  .grow { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .mono { font-family: ui-monospace, Consolas, monospace; font-size: 12px; }
  .dot { width: 8px; height: 8px; border-radius: 50%; background: #4b5563; }
  .dot.on { background: #22c55e; }
  nav { display: flex; gap: 6px; margin-bottom: 12px; flex-wrap: wrap; }
  nav button { background: #141b26; color: #cbd5e1; border: 1px solid #223; border-radius: 8px; padding: 6px 12px; }
  nav button.active { background: #1d4ed8; border-color: #1d4ed8; color: #fff; }
  .grid { display: grid; grid-template-columns: repeat(3, 1fr); gap: 8px; margin-bottom: 12px; }
  .metric { background: #101722; border: 1px solid #1e2937; border-radius: 10px; padding: 10px; }
  .metric .k { font-size: 12px; color: #8b98a8; }
  .metric .v { font-size: 20px; font-weight: 600; }
  .ok { color: #22c55e; } .bad { color: #ef4444; }
  .card { background: #0f1621; border: 1px solid #1e2937; border-radius: 12px; padding: 12px; margin-bottom: 12px; }
  .card h2 { margin: 0 0 8px; font-size: 14px; color: #9fb0c3; font-weight: 600; }
  .row { display: flex; align-items: center; gap: 8px; padding: 6px 0; border-bottom: 1px solid #16202c; }
  .row:last-child { border-bottom: none; }
  .badge { font-size: 11px; padding: 1px 6px; border-radius: 999px; background: #14311f; color: #4ade80; }
  .badge.bad { background: #3a1518; color: #f87171; }
  input, button { font: inherit; }
  input { width: 100%; box-sizing: border-box; background: #0b1220; border: 1px solid #24344a; color: #e6edf3; border-radius: 8px; padding: 8px; margin-bottom: 8px; }
  button { background: #1d4ed8; color: #fff; border: none; border-radius: 8px; padding: 8px 14px; }
</style>
