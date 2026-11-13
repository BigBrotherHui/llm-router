// llm-router 边缘中继
//
// 拓扑：
//   桌面 agent ──(出站 WSS /agent)──► Relay Durable Object
//   手机 ──► 最近的 CF 边缘（/full/ 完整控制台，/client 精简版）
//
// DO 负责：
//   1. 缓存 agent 最近一次 snapshot（精简版秒开）
//   2. 广播增量
//   3. 把完整控制台的 HTTP API 调用通过 agent 长连接转发给桌面（RPC）
const enc = new TextEncoder();

function b64encode(bytes) {
  let s = "";
  const chunk = 0x8000;
  for (let i = 0; i < bytes.length; i += chunk) {
    s += String.fromCharCode.apply(null, bytes.subarray(i, i + chunk));
  }
  return btoa(s);
}
function b64decode(text) {
  const bin = atob(text || "");
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}
function cookieToken(request) {
  const raw = request.headers.get("cookie") || "";
  const m = raw.match(/(?:^|;\s*)relay_token=([^;]+)/);
  return m ? decodeURIComponent(m[1]) : "";
}
function tokenOf(request, url) {
  return (
    url.searchParams.get("token") ||
    (request.headers.get("authorization") || "").replace(/^Bearer\s+/i, "") ||
    cookieToken(request)
  );
}
function json(data, status = 200) {
  return new Response(JSON.stringify(data), {
    status,
    headers: { "content-type": "application/json" },
  });
}

export class Relay {
  constructor(state, env) {
    this.state = state;
    this.env = env;
    this.pending = new Map();
  }

  async fetch(request) {
    const url = new URL(request.url);

    // 内部诊断 ping 不需要 token。
    if (url.pathname.endsWith("/ping")) return new Response("pong");

    if (this.env.RELAY_TOKEN && tokenOf(request, url) !== this.env.RELAY_TOKEN) {
      return new Response("forbidden", { status: 403 });
    }

    // 普通 HTTP 拿缓存快照：手机首屏不用等 WebSocket 握手。
    if (url.pathname.endsWith("/snapshot")) {
      const snapshot = (await this.state.storage.get("snapshot")) || "";
      return new Response(snapshot || JSON.stringify({ type: "empty" }), {
        headers: { "content-type": "application/json", "cache-control": "no-store" },
      });
    }

    const upgrade = (request.headers.get("upgrade") || "").toLowerCase();
    if (upgrade === "websocket") {
      const pair = new WebSocketPair();
      const [client, server] = Object.values(pair);
      this.state.acceptWebSocket(server);
      const role = url.pathname.endsWith("/agent") ? "agent" : "client";
      server.serializeAttachment({ role });
      if (role === "client") {
        const snapshot = await this.state.storage.get("snapshot");
        if (snapshot) {
          try { server.send(snapshot); } catch (e) {}
        }
      }
      try { server.send(JSON.stringify({ type: "hello", role })); } catch (e) {}
      return new Response(null, { status: 101, webSocket: client });
    }

    // 其余请求 = 完整控制台的 API：通过 agent 长连接 RPC 到桌面。
    const agent = [...this.state.getWebSockets()].find((ws) => {
      const a = ws.deserializeAttachment() || {};
      return a.role === "agent";
    });
    if (!agent) return json({ error: "agent offline" }, 503);

    const id = crypto.randomUUID();
    const body = new Uint8Array(await request.arrayBuffer());
    const headers = {};
    request.headers.forEach((v, k) => {
      if (
        ![
          "host",
          "connection",
          "upgrade",
          "content-length",
          "accept-encoding",
        ].includes(k.toLowerCase())
      ) {
        headers[k] = v;
      }
    });
    const payload = {
      type: "req",
      id,
      method: request.method,
      path: url.pathname + url.search,
      headers,
      body: body.length ? b64encode(body) : "",
    };

    try {
      agent.send(JSON.stringify(payload));
    } catch (e) {
      return json({ error: "agent send failed" }, 502);
    }

    const answered = await new Promise((resolve) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        resolve(null);
      }, 30000);
      this.pending.set(id, (value) => {
        clearTimeout(timer);
        this.pending.delete(id);
        resolve(value);
      });
    });
    if (!answered) return json({ error: "agent timeout" }, 504);

    const respHeaders = new Headers(answered.headers || {});
    respHeaders.delete("content-length");
    respHeaders.delete("content-encoding");
    return new Response(b64decode(answered.body), {
      status: answered.status || 200,
      headers: respHeaders,
    });
  }

  async webSocketMessage(ws, message) {
    const { role } = ws.deserializeAttachment() || {};
    if (role === "agent") {
      let parsed = null;
      try { parsed = JSON.parse(message); } catch (e) {}
      if (parsed && parsed.type === "res" && parsed.id) {
        const fn = this.pending.get(parsed.id);
        if (fn) fn(parsed);
        return;
      }
      if (parsed && parsed.type === "snapshot") {
        await this.state.storage.put("snapshot", message);
      }
      for (const socket of this.state.getWebSockets()) {
        const info = socket.deserializeAttachment() || {};
        if (info.role === "client") {
          try { socket.send(message); } catch (e) {}
        }
      }
    }
  }

  async webSocketClose(ws) {
    try { ws.close(); } catch (e) {}
  }

  async webSocketError(ws) {
    try { ws.close(); } catch (e) {}
  }
}

export default {
  async fetch(request, env) {
    const url = new URL(request.url);

    if (url.pathname === "/health") {
      return json({ ok: true, edge: true, colo: request.cf ? request.cf.colo : null });
    }

    // 完整控制台入口（/app，避开静态资源目录和旧重定向规则）
    if (url.pathname === "/app" || url.pathname === "/app/") {
      const t = url.searchParams.get("token") || "";
      const asset = await env.ASSETS.fetch(
        new Request(new URL("/full/index.html", url), request)
      );
      if (!t) return asset;
      const html = await asset.text();
      const inject = `<script>(function(){var t=new URLSearchParams(location.search).get("token");if(!t)return;var of=window.fetch;window.fetch=function(u,o){try{var x=new URL(u,location.origin);if(x.origin===location.origin&&!x.searchParams.has("token"))x.searchParams.set("token",t);return of(x.toString(),o)}catch(e){return of(u,o)}};var OW=window.WebSocket;window.WebSocket=function(u,p){try{var x=new URL(u);if(x.origin===location.origin)x.searchParams.set("token",t);return new OW(x.toString(),p)}catch(e){return new OW(u,p)}}})();</script>`;
      const out = /<head[^>]*>/i.test(html)
        ? html.replace(/<head[^>]*>/i, (m) => m + inject)
        : inject + html;
      return new Response(out, {
        headers: {
          "content-type": "text/html; charset=utf-8",
          "cache-control": "no-cache",
          "x-worker": "app",
        },
      });
    }

    // 诊断
    if (url.pathname === "/where") {
      const id = env.RELAY.idFromName("main-apac");
      const started = Date.now();
      let ping = "error";
      try {
        const r = await env.RELAY.get(id, { locationHint: "apac" }).fetch("https://do/ping");
        ping = await r.text();
      } catch (e) { ping = String(e); }
      return json({
        edge: request.cf ? request.cf.colo : null,
        country: request.cf ? request.cf.country : null,
        city: request.cf ? request.cf.city : null,
        asn: request.cf ? request.cf.asn : null,
        do_ms: Date.now() - started,
        do: ping,
      });
    }

    // WebSocket（agent / 精简客户端）
    if (url.pathname === "/agent" || url.pathname === "/client" || url.pathname === "/snapshot") {
      const id = env.RELAY.idFromName("main-apac");
      return env.RELAY.get(id, { locationHint: "apac" }).fetch(request);
    }

    // 完整控制台的 API：RPC 到桌面
    const rpc = ["/stats", "/bootstrap", "/admin/", "/healthz", "/v1/", "/events"];
    if (rpc.some((p) => url.pathname === p || url.pathname.startsWith(p))) {
      if (env.RELAY_TOKEN && tokenOf(request, url) !== env.RELAY_TOKEN) {
        return new Response("forbidden", { status: 403 });
      }
      const id = env.RELAY.idFromName("main-apac");
      return env.RELAY.get(id, { locationHint: "apac" }).fetch(request);
    }

    // 其余静态资源（精简版 / 完整版）
    return env.ASSETS.fetch(request);
  },
};
