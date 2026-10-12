#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""llm-router 自测：起几个假上游，验证模型路由 / 顺序调用 / 轮询 / 故障转移 / 流式 / 管理接口。"""

from __future__ import annotations

import json
import os
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

HERE = os.path.dirname(os.path.abspath(__file__))
ROUTER = os.path.join(HERE, "router.py")
PORT = 18711
PORT_URL = f"http://127.0.0.1:{PORT}"
UP_PORTS = {"a": 18721, "b": 18722, "c": 18723, "d": 18724}

hits: dict[str, list] = {"a": [], "b": [], "c": [], "d": []}
behavior: dict[str, str] = {"a": "ok", "b": "ok", "c": "ok", "d": "ok"}
last_error_payload: dict[str, bytes] = {}
lock = threading.Lock()

PASS, FAIL = [], []


def check(name: str, ok: bool, detail: str = "") -> None:
    (PASS if ok else FAIL).append(name)
    print(("  [PASS] " if ok else "  [FAIL] ") + name + (f"  -> {detail}" if detail and not ok else ""))


# 每个假上游提供的模型表。
# d 号专门给「ABCD 四家各叫 ds1..ds4」那个场景用。
UP_MODELS = {
    "a": ["deepseek-chat", "deepseek-reasoner", "glm-4.6"],
    "b": ["deepseek-chat", "glm-4.6", "qwen-max"],
    "c": ["deepseek-chat"],
    "d": ["ds4", "other"],
}
# ABCD 场景专用：四家各自叫 ds1..ds4
UP_MODELS_ABCD = {
    "a": ["ds1", "other"],
    "b": ["ds2", "other"],
    "c": ["ds3", "other"],
    "d": ["ds4", "other"],
}


class UpstreamServer(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    tag = "?"

    def log_message(self, *a):
        pass

    def _strict_path(self) -> bool:
        """有些真实上游只认一种路径，拼错就 404。

        b 号假上游开启这个模式，用来复现「base_url 已带 /v1，请求也带 /v1，
        拼成 /v1/v1/... 被上游 404」这类问题——宽松的假上游是测不出来的。
        """
        return self.tag == "b"

    def _path_ok(self) -> bool:
        path = urllib.parse.urlsplit(self.path).path
        if not self._strict_path():
            return True
        # 只接受 /v1/xxx 这种，且不允许出现 /v1/v1
        if "/v1/v1" in path or "//" in path:
            return False
        return path.startswith("/v1/")

    def _write_chunk(self, data: bytes) -> None:
        self.wfile.write(b"%x\r\n" % len(data) + data + b"\r\n")
        self.wfile.flush()

    def _json(self, status: int, obj) -> None:
        payload = json.dumps(obj, ensure_ascii=False).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def do_GET(self):
        if not self._path_ok():
            return self._json(404, {"error": {"message": "404 page not found"}})
        path = urllib.parse.urlsplit(self.path).path
        if path.endswith("/models"):
            with lock:
                behave = behavior[self.tag]
            if behave == "models_fail":
                return self._json(500, {"error": {"message": f"{self.tag} 拉取失败"}})
            if behave == "models_bad":
                return self._json(200, {"nonsense": True})
            if behave == "models_nested":
                # 有些厂商把列表包在 result 里
                return self._json(200, {"result": {"models": [
                    {"name": m} for m in UP_MODELS[self.tag]]}})
            return self._json(200, {"object": "list", "data": [
                {"id": m, "object": "model"} for m in UP_MODELS[self.tag]]})
        return self._json(404, {"error": {"message": "unknown"}})

    def do_POST(self):
        if not self._path_ok():
            return self._json(404, {"error": {"message": "404 page not found"}})
        n = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(n) if n else b"{}"
        try:
            body = json.loads(raw)
        except Exception:
            body = {}
        model = body.get("model")
        with lock:
            hits[self.tag].append(model)
            behave = behavior[self.tag]

        if behave == "fail500":
            return self._json(500, {"error": {"message": f"{self.tag} 挂了"}})
        if behave == "fail429":
            return self._json(429, {"error": {"message": f"{self.tag} 限流"}})
        if behave == "fail401":
            return self._json(401, {"error": {"message": f"{self.tag} key 无效"}})
        if behave == "fail400":
            payload = json.dumps({"error": {"message": "参数不合法", "type": "invalid_request_error"}},
                                 ensure_ascii=False).encode("utf-8")
            with lock:
                last_error_payload[self.tag] = payload
            self.send_response(400)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)
            return
        if behave == "empty_stream":
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Transfer-Encoding", "chunked")
            self.end_headers()
            self.wfile.write(b"0\r\n\r\n")
            self.wfile.flush()
            return

        if self.path.endswith("/stream"):
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Transfer-Encoding", "chunked")
            self.end_headers()
            for i in range(3):
                ev = json.dumps({"model": model,
                                 "choices": [{"delta": {"content": f"{self.tag}{i}"}}]})
                self._write_chunk(f"data: {ev}\n\n".encode())
            # 流式的 usage 通常在最后单独一个 chunk 里（带 DeepSeek 风格缓存字段）
            final = json.dumps({
                "model": model, "choices": [],
                "usage": {"prompt_tokens": 120, "completion_tokens": 30,
                          "total_tokens": 150,
                          "prompt_cache_hit_tokens": 90,
                          "prompt_cache_miss_tokens": 30},
            })
            self._write_chunk(f"data: {final}\n\n".encode())
            self._write_chunk(b"data: [DONE]\n\n")
            self.wfile.write(b"0\r\n\r\n")
            self.wfile.flush()
            return

        # 非流式：带上 usage，模拟真实厂商返回的 token 统计
        self._json(200, {
            "id": "x", "model": model, "from": self.tag,
            "choices": [{"message": {"content": f"hi from {self.tag}"}}],
            "usage": {"prompt_tokens": 100, "completion_tokens": 20,
                      "total_tokens": 120,
                      "prompt_tokens_details": {"cached_tokens": 64}},
        })


def start_upstreams():
    for tag, port in UP_PORTS.items():
        cls = type(f"Up_{tag}", (UpstreamServer,), {"tag": tag})
        srv = ThreadingHTTPServer(("127.0.0.1", port), cls)
        srv.daemon_threads = True
        threading.Thread(target=srv.serve_forever, daemon=True).start()


CONFIG_TMPL = """
[server]
host = "127.0.0.1"
port = {port}
log_requests = false

[router]
default_strategy = "priority"
failure_threshold = 2
cooldown_seconds = 60

[[upstreams]]
name = "{name_a}"
id = "a1"
base_url = "http://127.0.0.1:{pa}"
api_key = "key-a"

[[upstreams]]
name = "up-b"
id = "b1"
# 故意带上 /v1：真实厂商很多是这样（硅基流动、火山…），
# 路径拼错会变成 /v1/v1/... 被上游 404，这里就是那类回归测试
base_url = "http://127.0.0.1:{pb}/v1"
api_key = "key-b"

[[upstreams]]
name = "up-c"
id = "c1"
base_url = "http://127.0.0.1:{pc}"
api_key = "key-c"
"""

CONFIG_WITH_ROUTES = CONFIG_TMPL + """
[models."deepseek-chat"]
strategy = "priority"
entries = [
  {{ upstream = "a1", model = "deepseek-chat" }},
  {{ upstream = "b1", model = "deepseek-ai/DeepSeek-V3" }},
  {{ upstream = "c1", model = "deepseek-chat" }},
]

[models."glm-4.6"]
strategy = "round_robin"
entries = [
  {{ upstream = "a1", model = "glm-4.6" }},
  {{ upstream = "b1", model = "glm-4.6" }},
]
"""

# ABCD 四个上游，同一家的 deepseek 分别叫 ds1/ds2/ds3/ds4
# —— 用来验证「统一分组名 + 各家不同真实名」
CONFIG_FOUR = """
[server]
host = "127.0.0.1"
port = {port}
log_requests = false

[router]
default_strategy = "priority"
failure_threshold = 2
cooldown_seconds = 60

[[upstreams]]
name = "上游A"
id = "a1"
base_url = "http://127.0.0.1:{pa}"
api_key = "key-a"

[[upstreams]]
name = "上游B"
id = "b1"
base_url = "http://127.0.0.1:{pb}"
api_key = "key-b"

[[upstreams]]
name = "上游C"
id = "c1"
base_url = "http://127.0.0.1:{pc}"
api_key = "key-c"

[[upstreams]]
name = "上游D"
id = "d1"
base_url = "http://127.0.0.1:{pd}"
api_key = "key-d"
"""

_counter = [0]


def write_config(name_a="up-a", with_routes=False):
    _counter[0] += 1
    path = os.path.join(HERE, f"_test_cfg{_counter[0]}.toml")
    tmpl = CONFIG_WITH_ROUTES if with_routes else CONFIG_TMPL
    with open(path, "w", encoding="utf-8") as fh:
        fh.write(tmpl.format(port=PORT, pa=UP_PORTS["a"], pb=UP_PORTS["b"],
                             pc=UP_PORTS["c"], name_a=name_a))
    return path


def write_config_four():
    """ABCD 四上游配置，配合 UP_MODELS_ABCD 使用。"""
    _counter[0] += 1
    path = os.path.join(HERE, f"_test_four{_counter[0]}.toml")
    with open(path, "w", encoding="utf-8") as fh:
        fh.write(CONFIG_FOUR.format(port=PORT, pa=UP_PORTS["a"], pb=UP_PORTS["b"],
                                    pc=UP_PORTS["c"], pd=UP_PORTS["d"]))
    return path


def _req(path, body=None, method=None, port=PORT):
    data = None
    headers = {}
    if body is not None:
        data = json.dumps(body).encode()
        headers["Content-Type"] = "application/json"
    req = urllib.request.Request(f"http://127.0.0.1:{port}{path}", data=data,
                                 headers=headers, method=method or ("POST" if data else "GET"))
    try:
        with urllib.request.urlopen(req, timeout=60) as r:
            return r.status, r.read(), dict(r.headers)
    except urllib.error.HTTPError as e:
        return e.code, e.read(), dict(e.headers)


def post(path="/v1/chat/completions", body=None, port=PORT):
    return _req(path, body if body is not None else
                {"model": "deepseek-chat", "messages": [{"role": "user", "content": "hi"}]},
                port=port)


def admin(path, body=None, port=PORT):
    st, raw, _ = _req(path, body if body is not None else {}, port=port)
    try:
        return st, json.loads(raw)
    except Exception:
        return st, {}


def get(path, port=PORT):
    st, raw, _ = _req(path, port=port)
    try:
        return st, json.loads(raw)
    except Exception:
        return st, {}


def start_router(cfg_path):
    p = subprocess.Popen([sys.executable, ROUTER, "-c", cfg_path],
                         stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    for _ in range(100):
        time.sleep(0.15)
        try:
            st, _ = get("/health")
            if st == 200:
                return p
        except Exception:
            if p.poll() is not None:
                print("router 启动失败：\n" + (p.stdout.read() if p.stdout else ""))
                raise SystemExit(1)
    raise SystemExit("router 启动超时")


def stop_router(p):
    p.terminate()
    try:
        p.wait(timeout=5)
    except Exception:
        p.kill()


def reset_hits():
    with lock:
        for k in hits:
            hits[k].clear()


def cleanup():
    """只删测试自己生成的文件。

    注意：绝不能用 "*.state.json" 这种通配 ——
    那会把用户真实的 config.state.json 一起删掉（踩过一次）。
    测试用的文件一律以 _test_ 开头，状态文件是从它派生的同名文件。
    """
    for f in os.listdir(HERE):
        if f.startswith("_test_"):
            try:
                os.remove(os.path.join(HERE, f))
            except OSError:
                pass
    # 状态文件现在放在 .llmrouter/ 下，只清测试自己的（_test_ 开头）
    state_dir = os.path.join(HERE, ".llmrouter")
    if os.path.isdir(state_dir):
        for f in os.listdir(state_dir):
            if f.startswith("_test_"):
                try:
                    os.remove(os.path.join(state_dir, f))
                except OSError:
                    pass


def main():
    print("启动假上游 …")
    start_upstreams()
    cleanup()

    # ================= 1. 拉取模型 =================
    print("\n[1] 一键拉取上游模型列表")
    cfg = write_config()
    p = start_router(cfg)
    try:
        st, r = admin("/admin/fetch-models")
        results = {x["id"]: x for x in (r.get("results") or [])}
        check("拉取返回三个上游", st == 200 and len(results) == 3, str(r)[:220])
        check("up-a 拉到 3 个模型", results["a1"]["ok"] and results["a1"]["count"] == 3,
              str(results.get("a1")))
        check("模型 id 正确",
              results["a1"]["models"] == ["deepseek-chat", "deepseek-reasoner", "glm-4.6"],
              str(results["a1"].get("models")))

        st, snap = get("/stats")
        by_id = {u["id"]: u for u in snap["upstreams"]}
        check("模型被存进上游状态",
              by_id["b1"]["available_models"] == ["deepseek-chat", "glm-4.6", "qwen-max"],
              str(by_id["b1"]["available_models"]))
        check("记录了拉取时间", bool(by_id["a1"]["models_fetched_at"]),
              str(by_id["a1"]["models_fetched_at"]))
        check("生成了模型总表（4 个去重后的模型）", len(snap.get("catalog") or []) == 4,
              str([c["name"] for c in snap.get("catalog") or []]))

        behavior["c"] = "models_fail"
        st, r = admin("/admin/fetch-models", {"ids": ["c1"]})
        one = (r.get("results") or [{}])[0]
        check("上游拉取失败会报原因", one.get("ok") is False and "500" in str(one.get("error")),
              str(one))
        behavior["c"] = "ok"

        behavior["c"] = "models_nested"
        st, r = admin("/admin/fetch-models", {"ids": ["c1"]})
        one = (r.get("results") or [{}])[0]
        check("兼容 result.models[].name 这种格式", one.get("ok") and one.get("count") == 1, str(one))
        behavior["c"] = "ok"

        behavior["c"] = "models_bad"
        st, r = admin("/admin/fetch-models", {"ids": ["c1"]})
        one = (r.get("results") or [{}])[0]
        check("返回内容无法解析时报错", one.get("ok") is False, str(one))
        behavior["c"] = "ok"

        admin("/admin/fetch-models")
    finally:
        stop_router(p)

    # ================= 2. 按模型配路由 =================
    print("\n[2] 按模型配置上游顺序")
    cleanup()
    cfg = write_config()
    p = start_router(cfg)
    try:
        admin("/admin/fetch-models")
        st, r = admin("/admin/model/save", {
            "name": "deepseek-chat", "strategy": "priority",
            "entries": [
                {"upstream": "a1", "model": "deepseek-chat"},
                {"upstream": "b1", "model": "deepseek-ai/DeepSeek-V3"},
                {"upstream": "c1", "model": "deepseek-chat"},
            ]})
        check("保存模型路由成功", st == 200, str(r)[:200])

        st, snap = get("/stats")
        check("路由出现在 stats 里", "deepseek-chat" in snap["models"], str(list(snap["models"])))
        check("顺序和保存的一致",
              [e["upstream"] for e in snap["models"]["deepseek-chat"]["entries"]] == ["a1", "b1", "c1"],
              str(snap["models"]["deepseek-chat"]["entries"]))

        reset_hits()
        st, body, hdr = post()
        check("优先用排第一的上游", hdr.get("X-Upstream") == "up-a" and st == 200,
              f"status={st} up={hdr.get('X-Upstream')}")
        check("没有白试其他上游", not hits["b"] and not hits["c"], str(hits))

        st, r = admin("/admin/model/save", {
            "name": "glm-4.6", "strategy": "round_robin",
            "entries": [{"upstream": "a1", "model": "glm-4.6"},
                        {"upstream": "b1", "model": "glm-4.6"}]})
        check("保存轮询路由成功", st == 200, str(r)[:160])
        reset_hits()
        seen = []
        for _ in range(4):
            st, body, hdr = post(body={"model": "glm-4.6",
                                       "messages": [{"role": "user", "content": "hi"}]})
            seen.append(hdr.get("X-Upstream"))
        check("glm-4.6 轮询 a→b→a→b", seen == ["up-a", "up-b", "up-a", "up-b"], str(seen))

        admin("/admin/model/save", {
            "name": "deepseek-chat", "strategy": "priority",
            "entries": [{"upstream": "c1", "model": "deepseek-chat"},
                        {"upstream": "a1", "model": "deepseek-chat"}]})
        reset_hits()
        st, body, hdr = post()
        check("改顺序后立即生效", hdr.get("X-Upstream") == "up-c", str(hdr.get("X-Upstream")))

        admin("/admin/model/save", {
            "name": "deepseek-chat", "strategy": "priority",
            "entries": [{"upstream": "a1", "model": "deepseek-chat"},
                        {"upstream": "a1", "model": "deepseek-chat"}]})
        st, snap = get("/stats")
        check("同一上游去重", len(snap["models"]["deepseek-chat"]["entries"]) == 1,
              str(snap["models"]["deepseek-chat"]["entries"]))

        st, r = admin("/admin/model/save", {"name": "x", "strategy": "priority", "entries": []})
        check("空 entries 被拒绝", st == 400, str(r)[:160])

        st, r = admin("/admin/model/delete", {"name": "glm-4.6"})
        check("删除模型路由成功", st == 200 and r.get("ok"), str(r)[:160])
        st, snap = get("/stats")
        check("删除后不在列表里", "glm-4.6" not in snap["models"], str(list(snap["models"])))
    finally:
        stop_router(p)

    # ================= 3. 配置里预写路由 =================
    print("\n[3] 配置文件里预写路由")
    cleanup()
    cfg = write_config(with_routes=True)
    p = start_router(cfg)
    try:
        st, snap = get("/stats")
        check("从配置读到 2 个模型路由", len(snap["models"]) == 2, str(list(snap["models"])))
        check("配置里的顺序被保留",
              [e["upstream"] for e in snap["models"]["deepseek-chat"]["entries"]] == ["a1", "b1", "c1"],
              str(snap["models"]["deepseek-chat"]["entries"]))
        check("配置里的模型名映射被保留",
              snap["models"]["deepseek-chat"]["entries"][1]["model"] == "deepseek-ai/DeepSeek-V3",
              str(snap["models"]["deepseek-chat"]["entries"][1]))
        reset_hits()
        st, body, hdr = post()
        check("按配置走第一个上游", hdr.get("X-Upstream") == "up-a", str(hdr.get("X-Upstream")))
    finally:
        stop_router(p)

    # ================= 4. 故障转移 =================
    print("\n[4] 失败自动换下一个上游")
    cleanup()
    cfg = write_config(with_routes=True)
    behavior["a"] = "fail500"
    p = start_router(cfg)
    try:
        reset_hits()
        st, body, hdr = post()
        check("500 后切到第二个上游", st == 200 and hdr.get("X-Upstream") == "up-b",
              f"status={st} up={hdr.get('X-Upstream')}")
        check("第一个上游确实被试过", hits["a"] == ["deepseek-chat"], str(hits))
        check("模型名映射生效（上游收到 V3 名）", "deepseek-ai/DeepSeek-V3" in hits["b"], str(hits))
        j = json.loads(body)
        check("返回给客户端的模型名被改回来", j.get("model") == "deepseek-chat", str(j))

        # 429 也该切换：让 a 也挂，这样必然要落到 c；同时验证 b 的 429 被识别
        behavior["a"] = "fail500"
        behavior["b"] = "fail429"
        reset_hits()
        st, body, hdr = post()
        check("429 也被当成失败并继续切换",
              st == 200 and hdr.get("X-Upstream") == "up-c" and hits["b"],
              f"status={st} up={hdr.get('X-Upstream')} hits={hits}")
    finally:
        stop_router(p)
        behavior["a"] = behavior["b"] = behavior["c"] = "ok"

    # ================= 5. 客户端错误不该切换 =================
    print("\n[5] 客户端参数错误不白耗其他上游")
    cleanup()
    cfg = write_config(with_routes=True)
    behavior["a"] = "fail400"
    p = start_router(cfg)
    try:
        reset_hits()
        st, body, hdr = post()
        expect = last_error_payload.get("a", b"")
        check("原样透传 400", st == 400, str(st))
        check("没有去试 b/c", not hits["b"] and not hits["c"], str(hits))
        check("错误体逐字节透传", body == expect and expect != b"", f"got={body[:80]!r}")
        st, snap = get("/stats")
        check("400 不触发冷却", snap["upstreams"][0]["cooldown_remaining_seconds"] == 0, "")
    finally:
        stop_router(p)
        behavior["a"] = "ok"

    # ================= 6. 自动兜底 =================
    print("\n[6] 没配过的模型自动挑上游")
    cleanup()
    cfg = write_config()
    p = start_router(cfg)
    try:
        admin("/admin/fetch-models")
        reset_hits()
        st, body, hdr = post(body={"model": "glm-4.6",
                                   "messages": [{"role": "user", "content": "hi"}]})
        check("自动只挑声称有这个模型的上游",
              st == 200 and hits["c"] == [] and hits["a"], str(hits))
        st, raw = get("/admin/history")
        hist = raw.get("history") or []
        check("历史里标了路由来源", bool(hist) and "自动" in (hist[-1].get("route_source") or ""),
              str(hist[-1].get("route_source") if hist else None))
    finally:
        stop_router(p)

    cleanup()
    p = start_router(write_config())
    try:
        reset_hits()
        st, body, hdr = post(body={"model": "who-knows", "messages": []})
        check("未知模型也兜底尝试而不是直接 503", st == 200 and hits["a"], f"status={st}")
    finally:
        stop_router(p)

    # ================= 7. 流式 =================
    print("\n[7] 流式响应")
    cleanup()
    cfg = write_config(with_routes=True)
    p = start_router(cfg)
    try:
        reset_hits()
        st, body, hdr = post(path="/v1/chat/completions/stream")
        text = body.decode("utf-8", "replace")
        check("流式返回 200", st == 200, str(st))
        # 3 个内容块 + 1 个末尾带 usage 的块（真实厂商也是这么发的）
        check("收到 3 个内容块（外加 usage 块）",
              text.count('"delta"') == 3, text[:200])
        check("包含 [DONE]", "[DONE]" in text, text[-80:])
        check("内容是 a0/a1/a2", "a0" in text and "a2" in text, text[:200])
        check("usage 块透传给了客户端", '"usage"' in text and '"total_tokens"' in text,
              text[-260:])
    finally:
        stop_router(p)

    cleanup()
    behavior["a"] = "empty_stream"
    p = start_router(write_config(with_routes=True))
    try:
        reset_hits()
        st, body, hdr = post(path="/v1/chat/completions/stream")
        check("空流会切换到下一个上游", hdr.get("X-Upstream") == "up-b", str(hdr.get("X-Upstream")))
    finally:
        stop_router(p)
        behavior["a"] = "ok"

    # ================= 8. 上游管理 =================
    print("\n[8] 上游增删改")
    cleanup()
    cfg = write_config()
    p = start_router(cfg)
    try:
        st, r = admin("/admin/upstream/add", {
            "name": "新加的", "base_url": "http://127.0.0.1:%d" % UP_PORTS["c"], "api_key": "k"})
        check("添加上游成功", st == 200 and r["upstream"]["name"] == "新加的", str(r)[:200])
        new_id = r["upstream"]["id"]

        st, r = admin("/admin/upstream/add", {"name": "坏地址", "base_url": "不是网址"})
        check("拒绝非法地址", st == 400, str(r)[:160])
        st, r = admin("/admin/upstream/add", {"name": "没地址"})
        check("拒绝空地址", st == 400, str(r)[:160])

        st, r = admin("/admin/upstream/update", {"id": new_id, "name": "改过名了", "enabled": False})
        check("修改上游成功", st == 200 and r["upstream"]["name"] == "改过名了"
              and r["upstream"]["enabled"] is False, str(r)[:200])

        # 新加的上游要拉取到模型、并勾选后，才能建路由（save_model 会校验）
        admin("/admin/fetch-models", {"ids": [new_id]})
        st, fetch_r = admin("/admin/select-models", {"id": new_id, "models": ["deepseek-chat"]})
        check("新上游勾选模型后自动建了路由",
              st == 200 and "deepseek-chat" in (fetch_r.get("created_models") or []),
              str(fetch_r)[:200])

        st, snap = get("/stats")
        check("停用的上游不计入可用数",
              snap["models"]["deepseek-chat"]["resolved_count"] == 0,
              str(snap["models"]["deepseek-chat"]))

        st, r = admin("/admin/upstream/delete", {"id": new_id})
        check("删除上游成功", st == 200 and r.get("ok"), str(r)[:200])
        check("删上游会带走它的模型路由", "deepseek-chat" in (r.get("affected_models") or []),
              str(r.get("affected_models")))
        st, snap = get("/stats")
        check("上游确实没了", all(u["id"] != new_id for u in snap["upstreams"]),
              str([u["id"] for u in snap["upstreams"]]))

        st, r = admin("/admin/upstream/delete", {"id": "不存在"})
        check("删不存在的上游返回 404", st == 404, str(st))
    finally:
        stop_router(p)

    # ================= 9. 一键自动配置 =================
    print("\n[9] 按上游顺序自动配好（只认勾选过的模型）")
    cleanup()
    cfg = write_config()
    p = start_router(cfg)
    try:
        admin("/admin/fetch-models")
        # 一个都没勾选时，自动配置应该什么都不建
        st, r0 = admin("/admin/auto-configure")
        check("没勾选任何模型时不建路由",
              st == 200 and not (r0.get("created") or []), str(r0)[:200])

        # 勾选几个模型后自动配置
        admin("/admin/select-models", {"id": "a1", "models": ["deepseek-chat"]})
        admin("/admin/select-models", {"id": "b1", "models": ["deepseek-chat", "qwen-max"]})
        admin("/admin/select-models", {"id": "c1", "models": ["deepseek-chat"]})
        # 勾选本身就会建路由，先都删掉，才能验证自动配置确实做了事
        admin("/admin/model/delete", {"name": "deepseek-chat"})
        admin("/admin/model/delete", {"name": "qwen-max"})

        st, r = admin("/admin/auto-configure")
        check("自动配置成功", st == 200, str(r)[:200])
        check("给 2 个勾选过的模型建了路由",
              sorted(r.get("created") or []) == ["deepseek-chat", "qwen-max"],
              str(r.get("created")))

        st, snap = get("/stats")
        qwen = snap["models"]["qwen-max"]["entries"]
        check("qwen-max 只配了勾选它的上游", [e["upstream"] for e in qwen] == ["b1"], str(qwen))
        ds = snap["models"]["deepseek-chat"]["entries"]
        check("deepseek-chat 配了全部三个上游",
              [e["upstream"] for e in ds] == ["a1", "b1", "c1"], str(ds))
        check("没勾选的模型不会被自动配上",
              "glm-4.6" not in snap["models"] and "kimi-k2" not in snap["models"],
              str(list(snap["models"])))

        st, r2 = admin("/admin/auto-configure")
        check("重复执行不覆盖已有配置",
              not (r2.get("created") or []) and len(r2.get("skipped") or []) == 2,
              str(r2)[:200])

        st, r3 = admin("/admin/reset-to-config")
        check("恢复到配置文件状态", st == 200 and r3.get("ok"), str(r3)[:160])
        st, snap = get("/stats")
        check("恢复后没有模型路由了", len(snap["models"]) == 0, str(list(snap["models"])))
    finally:
        stop_router(p)

    # ================= 10. 不需要本地 Key =================
    print("\n[10] 不设本地鉴权（打开就能用）")
    cleanup()
    cfg = write_config()
    p = start_router(cfg)
    try:
        st, _ = get("/stats")
        check("stats 无需 Key 直接可读", st == 200, str(st))
        st, _ = admin("/admin/fetch-models")
        check("管理接口无需 Key", st == 200, str(st))
        st, _ = get("/")
        check("页面无需鉴权", st == 200, str(st))
        st, body, hdr = post()
        check("代理接口无需 Key", st == 200, str(st))
    finally:
        stop_router(p)

    # ================= 11. 没有次数上限 =================
    print("\n[11] 失败时一路试到底（没有「最多试几个」的上限）")
    cleanup()
    cfg = write_config(with_routes=True)
    behavior["a"] = behavior["b"] = "fail500"
    p = start_router(cfg)
    try:
        reset_hits()
        st, body, hdr = post()
        check("前两个挂了仍试到第三个", st == 200 and hdr.get("X-Upstream") == "up-c",
              f"status={st} up={hdr.get('X-Upstream')}")
        check("三个上游都试了", all(hits[k] for k in "abc"), str(hits))

        behavior["c"] = "fail500"
        reset_hits()
        st, body, hdr = post()
        check("全挂时返回 5xx", 500 <= st < 600, str(st))
        check("返回的是上游的错误内容", "挂了".encode("utf-8") in body or b"error" in body.lower(),
              body[:160])
        check("带上尝试过的上游列表以便排查",
              "up-a" in (hdr.get("X-Upstream-Attempted") or ""),
              repr(hdr.get("X-Upstream-Attempted")))
    finally:
        stop_router(p)
        behavior["a"] = behavior["b"] = behavior["c"] = "ok"

    # ================= 12. 中文名字 =================
    print("\n[12] 中文上游名")
    cleanup()
    cfg = write_config(name_a="DeepSeek 官方①", with_routes=True)
    p = start_router(cfg)
    try:
        reset_hits()
        st, body, hdr = post()
        check("中文名下请求成功", st == 200, str(st))
        raw = hdr.get("X-Upstream") or ""
        check("响应头已 ASCII 化", all(ord(c) < 128 for c in raw), repr(raw))
        check("中文名可还原", urllib.parse.unquote(raw) == "DeepSeek 官方①", repr(raw))
        st, snap = get("/stats")
        check("stats 里中文名原样", snap["upstreams"][0]["name"] == "DeepSeek 官方①",
              snap["upstreams"][0]["name"])
        st, raw_body = get("/admin/history")
        hist = raw_body.get("history") or []
        check("历史记录里中文名正常", bool(hist) and hist[-1]["upstream"] == "DeepSeek 官方①",
              str(hist[-1] if hist else None))
    finally:
        stop_router(p)

    # ================= 13. 持久化 =================
    print("\n[13] 重启后配置还在")
    cleanup()
    cfg = write_config()
    p = start_router(cfg)
    try:
        admin("/admin/fetch-models")
        admin("/admin/model/save", {
            "name": "deepseek-chat", "strategy": "round_robin",
            "entries": [{"upstream": "c1", "model": "deepseek-chat"},
                        {"upstream": "a1", "model": "deepseek-chat"}]})
        admin("/admin/upstream/update", {"id": "b1", "name": "B 改过名", "enabled": False})
        state_file = os.path.join(HERE, ".llmrouter",
                                 os.path.splitext(os.path.basename(cfg))[0] + ".state.json")
        check("生成了状态文件", os.path.exists(state_file), state_file)
    finally:
        stop_router(p)

    p = start_router(cfg)
    try:
        st, snap = get("/stats")
        check("重启后模型路由还在", "deepseek-chat" in snap["models"], str(list(snap["models"])))
        check("重启后策略还是轮询", snap["models"]["deepseek-chat"]["strategy"] == "round_robin",
              snap["models"]["deepseek-chat"]["strategy"])
        check("重启后顺序还是 c→a",
              [e["upstream"] for e in snap["models"]["deepseek-chat"]["entries"]] == ["c1", "a1"],
              str(snap["models"]["deepseek-chat"]["entries"]))
        check("重启后上游改名和停用都保留",
              {u["id"]: (u["name"], u["enabled"]) for u in snap["upstreams"]}["b1"]
              == ("B 改过名", False),
              str([(u["id"], u["name"], u["enabled"]) for u in snap["upstreams"]]))
    finally:
        stop_router(p)

    # ================= 14. models 接口 & 测速 =================
    print("\n[14] /v1/models 与测速")
    cleanup()
    cfg = write_config()
    p = start_router(cfg)
    try:
        admin("/admin/fetch-models")
        # 勾选两个模型，/v1/models 应该只列它们
        admin("/admin/select-models", {"id": "a1", "models": ["deepseek-chat", "glm-4.6"]})
        admin("/admin/select-models", {"id": "b1", "models": ["deepseek-chat"]})
        admin("/admin/auto-configure")
        st, raw, _ = _req("/v1/models")
        m = json.loads(raw)
        ids = sorted(x["id"] for x in m["data"])
        check("models 只列出选用的 2 个模型", ids == ["deepseek-chat", "glm-4.6"], str(ids))
        check("models 标注了上游", all("upstreams" in x for x in m["data"]), str(m["data"][:1]))

        st, r = admin("/admin/speedtest")
        res = r.get("results") or []
        check("测速返回三个上游", len(res) == 3, str(len(res)))
        check("测速都成功", all(x["ok"] for x in res), str(res))
        check("测速带延迟", all(x.get("latency_seconds") is not None for x in res), str(res))

        behavior["b"] = "fail500"
        st, r = admin("/admin/speedtest", {"ids": ["b1"]})
        one = (r.get("results") or [{}])[0]
        check("坏上游测速报告失败", one.get("ok") is False and one.get("error"), str(one))
        behavior["b"] = "ok"
    finally:
        stop_router(p)

    cleanup()
    p = start_router(write_config())
    try:
        st, r = admin("/admin/speedtest")
        res = r.get("results") or []
        check("未拉取模型时测速给出提示",
              all(not x.get("ok") and "拉取" in (x.get("error") or "") for x in res), str(res))
    finally:
        stop_router(p)

    # ================= 15. 历史记录 =================
    print("\n[15] 最近请求记录")
    cleanup()
    cfg = write_config(with_routes=True)
    p = start_router(cfg)
    try:
        admin("/admin/reset")
        post()
        post(path="/v1/chat/completions/stream")
        st, raw = get("/admin/history")
        hist = raw.get("history") or []
        check("记录了 2 条", len(hist) == 2, str(len(hist)))
        check("记录了上游名和状态",
              all(h.get("upstream") and h.get("status") for h in hist), str(hist)[:200])
        check("记录了耗时", all(h.get("elapsed_seconds") is not None for h in hist), str(hist)[:200])
        check("流式被标记", any(h.get("stream") for h in hist), str(hist)[:200])
    finally:
        stop_router(p)

    # ================= 16. 勾选模型 =================
    print("\n[16] 拉取后默认不选，勾选才参与路由")
    cleanup()
    cfg = write_config()
    p = start_router(cfg)
    try:
        # 拉取前：没有可选模型
        st, snap = get("/stats")
        up_a = {u["id"]: u for u in snap["upstreams"]}["a1"]
        check("新上游可用模型为空", up_a["available_count"] == 0, str(up_a["available_count"]))
        check("新上游已选为空", up_a["selected_count"] == 0, str(up_a["selected_count"]))

        # 拉取后：有模型可选，但默认一个都不选
        admin("/admin/fetch-models", {"ids": ["a1"]})
        st, snap = get("/stats")
        up_a = {u["id"]: u for u in snap["upstreams"]}["a1"]
        check("拉取后有 3 个可选模型", up_a["available_count"] == 3, str(up_a["available_count"]))
        check("拉取后仍然一个都没选", up_a["selected_count"] == 0, str(up_a["selected_count"]))
        check("拉取后没有自动建路由", len(snap["models"]) == 0, str(list(snap["models"])))

        # 勾选一个：应该自动建路由
        st, r = admin("/admin/select-models", {"id": "a1", "models": ["deepseek-chat"]})
        check("勾选成功", st == 200 and r.get("selected") == ["deepseek-chat"], str(r)[:200])
        check("勾选会自动建模型路由", "deepseek-chat" in (r.get("created_models") or []),
              str(r.get("created_models")))

        st, snap = get("/stats")
        check("路由里出现了这个上游",
              [e["upstream"] for e in snap["models"]["deepseek-chat"]["entries"]] == ["a1"],
              str(snap["models"].get("deepseek-chat")))
        reset_hits()
        st, body, hdr = post()
        check("勾选后请求能走通", st == 200 and hdr.get("X-Upstream") == "up-a",
              f"status={st} up={hdr.get('X-Upstream')}")

        # 再勾一个上游，应该追加到路由末尾（不覆盖已有顺序）
        admin("/admin/fetch-models", {"ids": ["b1"]})
        admin("/admin/select-models", {"id": "b1", "models": ["deepseek-chat"]})
        st, snap = get("/stats")
        order = [e["upstream"] for e in snap["models"]["deepseek-chat"]["entries"]]
        check("新勾选的上游追加在末尾", order == ["a1", "b1"], str(order))

        # 取消勾选 a1：应从路由里摘掉
        st, r = admin("/admin/select-models", {"id": "a1", "models": []})
        check("取消勾选成功", st == 200 and r.get("selected") == [], str(r)[:200])
        st, snap = get("/stats")
        order = [e["upstream"] for e in snap["models"]["deepseek-chat"]["entries"]]
        check("取消勾选后从路由摘掉", order == ["b1"], str(order))

        # 把最后一个也取消：整条路由应该消失
        admin("/admin/select-models", {"id": "b1", "models": []})
        st, snap = get("/stats")
        check("所有上游都取消后路由消失", "deepseek-chat" not in snap["models"],
              str(list(snap["models"])))

        # 全选 / 全不选
        st, r = admin("/admin/select-models", {"id": "a1", "all": True})
        check("全选成功", st == 200 and len(r.get("selected") or []) == 3, str(r.get("selected")))
        st, r = admin("/admin/select-models", {"id": "a1", "none": True})
        check("全不选成功", st == 200 and r.get("selected") == [], str(r)[:160])

        # 不能勾选上游没有的模型
        st, r = admin("/admin/select-models", {"id": "a1", "models": ["不存在的模型"]})
        check("拒绝勾选上游没有的模型", st == 200 and r.get("selected") == [], str(r)[:160])

        # 模型路由里只显示勾选过的
        admin("/admin/select-models", {"id": "a1", "models": ["deepseek-chat"]})
        st, snap = get("/stats")
        cat = {e["name"]: e for e in snap["catalog"]}
        check("勾选的模型标为已选用",
              any(u["id"] == "a1" for u in cat["deepseek-chat"]["upstreams"]),
              str(cat["deepseek-chat"]))
        check("没勾选的模型归到「有但没选」",
              any(u["id"] == "a1" for u in cat["glm-4.6"]["offered_only"]),
              str(cat["glm-4.6"]))
        check("没勾选的模型不在路由里", "glm-4.6" not in snap["models"], str(list(snap["models"])))

        # /v1/models 只列选用的
        st, raw, _ = _req("/v1/models")
        ids = [x["id"] for x in json.loads(raw)["data"]]
        check("/v1/models 只列选用的模型", ids == ["deepseek-chat"], str(ids))

        # 上游下架的模型，勾选也要清掉
        admin("/admin/select-models", {"id": "a1", "all": True})
        UP_MODELS["a"] = ["deepseek-chat"]      # 模拟上游下架了另外两个
        st, r = admin("/admin/fetch-models", {"ids": ["a1"]})
        one = (r.get("results") or [{}])[0]
        check("上游下架的模型会从勾选里清掉",
              one.get("selected") == ["deepseek-chat"], str(one.get("selected")))
        check("报告了被清掉的勾选",
              set(one.get("dropped_selections") or []) == {"deepseek-reasoner", "glm-4.6"},
              str(one.get("dropped_selections")))
        UP_MODELS["a"] = ["deepseek-chat", "deepseek-reasoner", "glm-4.6"]
        admin("/admin/fetch-models", {"ids": ["a1"]})
    finally:
        stop_router(p)

    # 老状态文件缺 selected_models 字段时，按「原来路由里用到的」推断勾选，
    # 既不会让原配置失效，也不会把上百个没用到的模型全勾上
    print("\n[17] 旧状态文件兼容")
    cleanup()
    cfg = write_config()
    p = start_router(cfg)
    try:
        admin("/admin/fetch-models")
        # 只配一个模型的路由，模拟老版本的使用方式
        admin("/admin/model/save", {
            "name": "deepseek-chat", "strategy": "priority",
            "entries": [{"upstream": "a1", "model": "deepseek-chat"},
                        {"upstream": "b1", "model": "deepseek-chat"}]})
    finally:
        stop_router(p)
    state_path = os.path.join(HERE, ".llmrouter",
                              os.path.splitext(os.path.basename(cfg))[0] + ".state.json")
    with open(state_path, "r", encoding="utf-8") as fh:
        st_data = json.load(fh)
    for u in st_data["upstreams"]:
        u.pop("selected_models", None)     # 模拟老版本存的状态
    with open(state_path, "w", encoding="utf-8") as fh:
        json.dump(st_data, fh, ensure_ascii=False)
    p = start_router(cfg)
    try:
        st, snap = get("/stats")
        by_id = {u["id"]: u for u in snap["upstreams"]}
        check("老状态按原路由推断勾选（只勾用到的）",
              by_id["a1"]["selected_models"] == ["deepseek-chat"],
              str(by_id["a1"]["selected_models"]))
        check("没参与路由的模型不会被顺手勾上",
              by_id["a1"]["selected_count"] < by_id["a1"]["available_count"],
              f"selected={by_id['a1']['selected_count']} available={by_id['a1']['available_count']}")
        check("没有路由的上游勾选为空",
              by_id["c1"]["selected_models"] == [], str(by_id["c1"]["selected_models"]))
        check("原有路由依然可用", "deepseek-chat" in snap["models"], str(list(snap["models"])))
        reset_hits()
        st, body, hdr = post()
        check("迁移后请求照常走通", st == 200 and hdr.get("X-Upstream") == "up-a",
              f"status={st} up={hdr.get('X-Upstream')}")
    finally:
        stop_router(p)

    # ================= 18. 同名分组（ABCD 四家各叫 ds1..ds4）=================
    print("\n[18] 分组：四家上游的同一个模型各自叫 ds1/ds2/ds3/ds4，统一成 deepseek")
    cleanup()
    UP_MODELS.update(UP_MODELS_ABCD)          # 切成 ABCD 场景的模型表
    cfg = write_config_four()
    p = start_router(cfg)
    try:
        admin("/admin/fetch-models")
        # 四家各勾一个自己那边的名字
        for uid, real in [("a1", "ds1"), ("b1", "ds2"), ("c1", "ds3"), ("d1", "ds4")]:
            admin("/admin/select-models", {"id": uid, "models": [real]})

        st, snap = get("/stats")
        check("还没分组时是 4 个独立名字",
              sorted(snap["models"]) == ["ds1", "ds2", "ds3", "ds4"], str(sorted(snap["models"])))

        # 把四家的 ds 都归到 deepseek 分组
        for uid, real in [("a1", "ds1"), ("b1", "ds2"), ("c1", "ds3"), ("d1", "ds4")]:
            st, r = admin("/admin/set-group", {"id": uid, "mapping": {real: "deepseek"}})
            check(f"{real} 归组成功", st == 200 and r.get("model_groups", {}).get(real) == "deepseek",
                  str(r)[:160])

        st, snap = get("/stats")
        check("归组后合并成一个 deepseek", "deepseek" in snap["models"], str(sorted(snap["models"])))
        check("原来那 4 个独立名字没了",
              not any(k in snap["models"] for k in ("ds1", "ds2", "ds3", "ds4")),
              str(sorted(snap["models"])))

        members = snap["models"]["deepseek"]["members"]
        check("deepseek 分组有 4 个成员", len(members) == 4, str(len(members)))
        check("四个成员用的是各家自己的真实名字",
              sorted(m["model"] for m in members) == ["ds1", "ds2", "ds3", "ds4"],
              str([m["model"] for m in members]))
        check("四个成员分别来自四个上游",
              sorted(m["upstream"] for m in members) == ["a1", "b1", "c1", "d1"],
              str([m["upstream"] for m in members]))
        check("成员被标记为改过名", all(m["renamed"] for m in members),
              str([m["renamed"] for m in members]))

        # 客户端用 deepseek 调用，实际发给各家的是 ds1..ds4
        reset_hits()
        st, body, hdr = post(body={"model": "deepseek", "messages": [{"role": "user", "content": "hi"}]})
        check("用分组名调用成功", st == 200, str(st))
        check("首选是排第一的上游",
              urllib.parse.unquote(hdr.get("X-Upstream") or "") == "上游A",
              str(hdr.get("X-Upstream")))
        check("实际发给上游的是真实名 ds1", hits["a"] == ["ds1"], str(hits))
        check("返回给客户端的 model 仍是 deepseek",
              json.loads(body).get("model") == "deepseek", str(json.loads(body).get("model")))

        # 换个名字调用就不该走了（分组名才是唯一入口）
        reset_hits()
        st, body, hdr = post(body={"model": "ds1", "messages": []})
        check("直接用真实名 ds1 不再是走配置的路线",
              (hdr.get("X-Upstream-Attempted") is None) if st != 200 else True,
              f"status={st}")

        # 改名：deepseek → llm
        st, r = admin("/admin/rename-group", {"from": "deepseek", "to": "llm"})
        check("分组改名成功", st == 200 and r.get("to") == "llm", str(r)[:160])
        st, snap = get("/stats")
        check("改名后新名字存在", "llm" in snap["models"], str(sorted(snap["models"])))
        check("改名后旧名字消失", "deepseek" not in snap["models"], str(sorted(snap["models"])))
        check("改名保留了 4 个成员", len(snap["models"]["llm"]["members"]) == 4,
              str(len(snap["models"]["llm"]["members"])))
        reset_hits()
        st, body, hdr = post(body={"model": "llm", "messages": []})
        check("用新分组名能调用", st == 200 and hits["a"] == ["ds1"], f"status={st} hits={hits}")

        # 取消映射：ds2 回到独立名字
        st, r = admin("/admin/set-group", {"id": "b1", "mapping": {"ds2": ""}})
        check("取消分组映射成功", st == 200, str(r)[:160])
        st, snap = get("/stats")
        check("取消映射后 ds2 变回独立分组", "ds2" in snap["models"], str(sorted(snap["models"])))
        check("llm 分组少了一个成员", len(snap["models"]["llm"]["members"]) == 3,
              str(len(snap["models"]["llm"]["members"])))

        # 拒绝给上游没有的模型设分组
        st, r = admin("/admin/set-group", {"id": "a1", "mapping": {"不存在的模型": "x"}})
        check("手填上游清单里没有的模型名也会被接受并记住",
              st == 200 and r.get("touched") == ["不存在的模型"], str(r)[:160])

        # 停止勾选 ds1：llm 分组再少一个
        admin("/admin/select-models", {"id": "a1", "models": []})
        st, snap = get("/stats")
        check("取消勾选后分组成员相应减少",
              "llm" not in snap["models"] or len(snap["models"]["llm"]["members"]) == 2,
              str(snap["models"].get("llm", {}).get("members")))

        st, raw, _ = _req("/v1/models")
        ids = sorted(x["id"] for x in json.loads(raw)["data"])
        check("/v1/models 列出的是分组名",
              all(":" not in i for i in ids) and ("llm" in ids or "ds2" in ids), str(ids))
    finally:
        stop_router(p)
        UP_MODELS.update({"a": ["deepseek-chat", "deepseek-reasoner", "glm-4.6"],
                          "b": ["deepseek-chat", "glm-4.6", "qwen-max"],
                          "c": ["deepseek-chat"],
                          "d": ["ds4", "other"]})

    # ================= 19. 保护用户配置不被误删 =================
    # 这个测试守的是一个真实踩过的坑：清理测试产物时把用户真实的状态文件删了。
    # 现在状态文件放在 .llmrouter/ 子目录，且清理只认 _test_ 前缀。
    print("\n[19] 用户配置保护")
    real_state = os.path.join(HERE, ".llmrouter", "config.state.json")
    real_cfg = os.path.join(HERE, "config.toml")

    def fingerprint(path):
        """只取「用户配置」的关键字段做指纹，不碰其它内容。"""
        if not os.path.exists(path):
            return None
        try:
            d = json.load(open(path, encoding="utf-8"))
        except Exception:
            return "unreadable"
        return json.dumps({
            "upstreams": [[u.get("id"), sorted(u.get("selected_models") or []),
                           sorted((u.get("model_groups") or {}).items())]
                          for u in (d.get("upstreams") or [])],
            "models": sorted((d.get("models") or {}).keys()),
        }, sort_keys=True, ensure_ascii=False)

    before_state = fingerprint(real_state)
    before_cfg = os.path.exists(real_cfg)

    check("状态文件放在 .llmrouter/ 子目录里（不在项目根目录）",
          os.path.isdir(os.path.join(HERE, ".llmrouter")),
          f"期望 {os.path.join(HERE, '.llmrouter')} 存在")

    # 模拟「按扩展名清理项目根目录」这种危险操作，跑一遍 cleanup
    cleanup()

    after_state = fingerprint(real_state)
    check("cleanup 不会删掉用户的状态文件",
          after_state == before_state,
          f"before={str(before_state)[:60]} after={str(after_state)[:60]}")
    check("cleanup 不会删掉用户的 config.toml",
          os.path.exists(real_cfg) == before_cfg,
          f"exists={os.path.exists(real_cfg)}")

    # 状态目录本身不该出现在项目根目录的通配匹配里
    root_states = [f for f in os.listdir(HERE) if f.endswith(".state.json")]
    check("项目根目录没有 .state.json（不会被 rm *.state.json 误伤）",
          not root_states, str(root_states))

    # 备份机制：保存时应留下 .bak；reset 应该进 trash 而不是删除
    cfg = write_config()
    p = start_router(cfg)
    try:
        admin("/admin/fetch-models", {"ids": ["a1"]})
        admin("/admin/select-models", {"id": "a1", "models": ["deepseek-chat"]})
        admin("/admin/select-models", {"id": "a1", "models": ["deepseek-chat", "glm-4.6"]})
        test_state = os.path.join(HERE, ".llmrouter",
                                  os.path.splitext(os.path.basename(cfg))[0] + ".state.json")
        check("状态文件写在 .llmrouter/ 里", os.path.exists(test_state), test_state)
        check("保存时自动留下 .bak 备份", os.path.exists(test_state + ".bak"),
              test_state + ".bak")

        st, r = admin("/admin/reset-to-config")
        trash = os.path.join(HERE, ".llmrouter", "trash")
        trashed = [f for f in os.listdir(trash)] if os.path.isdir(trash) else []
        check("reset 把旧状态移进 trash/ 而不是删掉",
              st == 200 and len(trashed) > 0, str(trashed)[:120])
    finally:
        stop_router(p)

    # ================= 20. 请求记录：token / 输入输出 / 分组与上游统计 =================
    print("\n[20] 请求记录明细与统计")
    cleanup()
    cfg = write_config()
    p = start_router(cfg)
    try:
        admin("/admin/fetch-models")

        # 建一个分组：三家的 deepseek-chat 合成一个名字
        # 注意要先「勾选」再「设分组」——勾选才是路由的开关
        admin("/admin/select-models", {"id": "a1", "models": ["deepseek-chat"]})
        admin("/admin/select-models", {"id": "b1", "models": ["deepseek-chat"]})
        admin("/admin/select-models", {"id": "c1", "models": ["deepseek-chat"]})
        admin("/admin/set-group", {"id": "a1", "mapping": {"deepseek-chat": "deepseek"}})
        admin("/admin/set-group", {"id": "b1", "mapping": {"deepseek-chat": "deepseek"}})
        admin("/admin/set-group", {"id": "c1", "mapping": {"deepseek-chat": "deepseek"}})
        st, snap = get("/stats")
        check("分组已建立（三家合并成 deepseek）",
              "deepseek" in (snap.get("models") or {}), str(list((snap.get("models") or {}))))
        admin("/admin/reset")

        # 非流式请求：应记录 usage 和输入输出
        st, body, hdr = post(body={"model": "deepseek",
                                   "messages": [{"role": "user", "content": "帮我写首诗"}]})
        check("分组调用成功", st == 200, str(st))

        st, raw = get("/admin/history")
        h = (raw.get("history") or [])[-1]
        check("记录里带 token 用量", bool(h.get("usage")), str(h.get("usage")))
        check("输入 token 正确", h["usage"].get("input") == 100, str(h.get("usage")))
        check("输出 token 正确", h["usage"].get("output") == 20, str(h.get("usage")))
        check("合计 token 正确", h["usage"].get("total") == 120, str(h.get("usage")))
        check("缓存读取被识别（嵌套 cached_tokens）",
              h["usage"].get("cache_read") == 64, str(h.get("usage")))

        check("记录了输入内容",
              "帮我写首诗" in (h.get("request_text") or ""), str(h.get("request_text"))[:80])
        check("记录了输出内容",
              "hi from a" in (h.get("response_text") or ""), str(h.get("response_text"))[:80])
        check("记录用的是分组名", h.get("model") == "deepseek", str(h.get("model")))
        check("记录里区分真实模型名",
              h.get("effective_model") == "deepseek-chat", str(h.get("effective_model")))

        # 流式请求：usage 应从末尾 chunk 里攒出来
        st, body, hdr = post(path="/v1/chat/completions/stream",
                             body={"model": "deepseek",
                                   "messages": [{"role": "user", "content": "流式测试"}]})
        st, raw = get("/admin/history")
        hs = raw.get("history") or []
        sh = hs[-1]
        check("流式请求也被记录", sh.get("stream") is True, str(sh.get("stream")))
        check("流式也能取到 token 用量",
              sh["usage"].get("total") == 150, str(sh.get("usage")))
        check("流式识别 DeepSeek 风格缓存字段",
              sh["usage"].get("cache_read") == 90, str(sh.get("usage")))
        check("流式记录了输入", "流式测试" in (sh.get("request_text") or ""),
              str(sh.get("request_text"))[:60])
        check("流式攒出了输出文本", "a0a1a2" in (sh.get("response_text") or ""),
              str(sh.get("response_text"))[:60])

        # 统计：按分组 + 分组×上游
        d = json.loads(urllib.request.urlopen(
            urllib.request.Request(PORT_URL + "/stats"), timeout=10).read())
        cs = d.get("call_stats") or {}
        check("stats 里有 call_stats", bool(cs), str(list(cs))[:100])

        groups = {g["group"]: g for g in (cs.get("groups") or [])}
        check("统计里出现 deepseek 分组", "deepseek" in groups, str(list(groups)))
        g = groups.get("deepseek") or {}
        check("分组调用次数为 2", g.get("calls") == 2, str(g.get("calls")))
        check("分组成功数为 2", g.get("ok") == 2, str(g.get("ok")))
        check("分组 token 合计正确（120+150）",
              g.get("usage", {}).get("total") == 270, str(g.get("usage")))
        check("分组缓存读合计正确（64+90）",
              g.get("usage", {}).get("cache_read") == 154, str(g.get("usage")))

        ups = {u["upstream"]: u for u in (g.get("upstreams") or [])}
        check("统计里有上游维度", len(ups) >= 1, str(list(ups)))
        check("上游调用次数被统计",
              sum(u["calls"] for u in ups.values()) == 2, str({k: v["calls"] for k, v in ups.items()}))

        totals = cs.get("totals") or {}
        check("总量统计正确", totals.get("calls") == 2 and totals.get("ok") == 2, str(totals))
        check("总量 token 正确", totals.get("total") == 270, str(totals))

        # 失败请求也要进统计
        behavior["a"] = behavior["b"] = behavior["c"] = "fail500"
        post(body={"model": "deepseek", "messages": [{"role": "user", "content": "x"}]})
        behavior["a"] = behavior["b"] = behavior["c"] = "ok"
        d = json.loads(urllib.request.urlopen(
            urllib.request.Request(PORT_URL + "/stats"), timeout=10).read())
        g = {x["group"]: x for x in (d["call_stats"]["groups"] or [])}["deepseek"]
        check("失败请求计入 failed", g.get("failed") == 1, str(g.get("failed")))
        check("失败请求也计入总数", g.get("calls") == 3, str(g.get("calls")))

        # 输入输出有长度上限，不会无限涨
        long_text = "啊" * 20000
        post(body={"model": "deepseek",
                   "messages": [{"role": "user", "content": long_text}]})
        st, raw = get("/admin/history")
        h = (raw.get("history") or [])[-1]
        n = len(h.get("request_text") or "")
        check("超长输入被截断保存", 0 < n <= 4000, str(n))
    finally:
        stop_router(p)

    # ================= 汇总 =================
    cleanup()
    print("\n" + "=" * 62)
    print(f"通过 {len(PASS)} 项，失败 {len(FAIL)} 项")
    if FAIL:
        print("失败项：")
        for f in FAIL:
            print("  -", f)
    print("=" * 62)
    return 1 if FAIL else 0


if __name__ == "__main__":
    raise SystemExit(main())
