// llm-router 边缘中继
//
// 拓扑：
//   桌面 agent ──(出站 WSS /agent)──► Relay Durable Object ──► 手机 thin client (/client)
//
// DO 只做三件事：
//   1. 缓存 agent 最近一次 snapshot（新客户端一连上就拿到，不必等桌面回包）
//   2. 把 agent 推来的增量广播给所有客户端
//   3. 用共享 token 鉴权
export class Relay {
  constructor(state, env) {
    this.state = state;
    this.env = env;
  }

  async fetch(request) {
    const url = new URL(request.url);
    const token =
      url.searchParams.get("token") ||
      (request.headers.get("authorization") || "").replace(/^Bearer\s+/i, "");
    if (this.env.RELAY_TOKEN && token !== this.env.RELAY_TOKEN) {
      return new Response("forbidden", { status: 403 });
    }

    // 普通 HTTP 拿缓存快照：手机首屏不用等 WebSocket 握手。
    if (url.pathname.endsWith("/snapshot")) {
      const snapshot = (await this.state.storage.get("snapshot")) || "";
      return new Response(snapshot || JSON.stringify({ type: "empty" }), {
        headers: {
          "content-type": "application/json",
          "cache-control": "no-store",
        },
      });
    }

    const pair = new WebSocketPair();
    const [client, server] = Object.values(pair);
    this.state.acceptWebSocket(server);

    const role = url.pathname.endsWith("/agent") ? "agent" : "client";
    server.serializeAttachment({ role });

    if (role === "client") {
      // 新客户端先拿缓存快照，秒出
      const snapshot = await this.state.storage.get("snapshot");
      if (snapshot) {
        try { server.send(snapshot); } catch (e) {}
      }
    }
    try { server.send(JSON.stringify({ type: "hello", role })); } catch (e) {}

    return new Response(null, { status: 101, webSocket: client });
  }

  async webSocketMessage(ws, message) {
    const { role } = ws.deserializeAttachment() || {};
    if (role !== "agent") return;
    // agent 的帧直接广播；snapshot 额外落盘，供后来者秒出
    try {
      const parsed = JSON.parse(message);
      if (parsed && parsed.type === "snapshot") {
        await this.state.storage.put("snapshot", message);
      }
    } catch (e) {}
    for (const socket of this.state.getWebSockets()) {
      const info = socket.deserializeAttachment() || {};
      if (info.role === "client") {
        try { socket.send(message); } catch (e) {}
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
      return new Response(JSON.stringify({ ok: true, edge: true }), {
        headers: { "content-type": "application/json" },
      });
    }
    if (url.pathname === "/agent" || url.pathname === "/client" || url.pathname === "/snapshot") {
      const id = env.RELAY.idFromName("main-apac");
      // 固定放到亚太，避免 DO 落在欧洲导致国内手机跨洲往返。
      return env.RELAY.get(id, { locationHint: "apac" }).fetch(request);
    }
    return env.ASSETS.fetch(request);
  },
};
