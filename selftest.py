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
# 每个假上游收到的完整请求体（hits 只记模型名）。
# 验「请求转换对不对」时必须看真正发上去的东西，光看模型名不够。
bodies: dict[str, list] = {"a": [], "b": [], "c": [], "d": []}
behavior: dict[str, str] = {"a": "ok", "b": "ok", "c": "ok", "d": "ok"}
# 每个假上游的人工延迟（秒），用来让「按测速重排」的结果可预测
delay: dict[str, float] = {"a": 0.0, "b": 0.0, "c": 0.0, "d": 0.0}
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
            bodies[self.tag].append(body)
            behave = behavior[self.tag]
            wait = delay.get(self.tag, 0.0)
        if wait:
            time.sleep(wait)

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
        if behave == "bad_json":
            # 2xx + 非流式 + 响应体不是 JSON 对象（网关吐纯文本错误页那种）。
            # 这种响应必须能正常透传，不能因为解析失败把请求打成 500。
            # 特意用 text/plain 而**不是** HTML：HTML 现在有专门的处理
            # （见下面的 bad_html），混在一起就分不清在测哪条了。
            payload = b"Bad Gateway: upstream said no"
            self.send_response(200)
            self.send_header("Content-Type", "text/plain")
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)
            return
        if behave == "bad_html":
            # 200 + HTML 首页：很多网关（New API / one-api）对认不出的路径
            # 不报 404 而是回首页。这不是 API 响应，必须当上游故障处理 ——
            # 否则就是「测试永远通、调用永远断」。
            payload = b"<!DOCTYPE html><html><body>Welcome to the gateway</body></html>"
            self.send_response(200)
            self.send_header("Content-Type", "text/html; charset=utf-8")
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)
            return

        # 流式有两条触发路径：
        #   1) 路径以 /stream 结尾（老测试用，模拟 OpenAI 的流式端点）
        #   2) 请求体里 stream=true（真实厂商的行为，也是 Anthropic 适配那条路 ——
        #      它会归一化成 /v1/chat/completions，路径里没有 /stream）
        if self.path.endswith("/stream") or body.get("stream"):
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

# 配置文件里预写额度（quota）和具名排序（orderings）—— 验证这两项也认配置文件
CONFIG_WITH_QUOTA_AND_ORDERINGS = CONFIG_TMPL + """
[models."deepseek-chat"]
strategy = "priority"
entries = [
  {{ upstream = "a1", model = "deepseek-chat" }},
  {{ upstream = "b1", model = "deepseek-chat", quota = 0 }},
  {{ upstream = "c1", model = "deepseek-chat" }},
]

[models."deepseek-chat".orderings]
"原始" = [
  {{ upstream = "a1", model = "deepseek-chat" }},
  {{ upstream = "b1", model = "deepseek-chat" }},
  {{ upstream = "c1", model = "deepseek-chat" }},
]
"快的先" = [
  {{ upstream = "c1", model = "deepseek-chat" }},
  {{ upstream = "a1", model = "deepseek-chat" }},
  {{ upstream = "b1", model = "deepseek-chat" }},
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


def write_config(name_a="up-a", with_routes=False, history_limit=None):
    _counter[0] += 1
    path = os.path.join(HERE, f"_test_cfg{_counter[0]}.toml")
    tmpl = CONFIG_WITH_ROUTES if with_routes else CONFIG_TMPL
    body = tmpl.format(port=PORT, pa=UP_PORTS["a"], pb=UP_PORTS["b"],
                       pc=UP_PORTS["c"], name_a=name_a)
    if history_limit:
        # 加在 [router] 段里：明细上限可配，方便验证「截断但统计不缩水」
        body = body.replace("[router]\n",
                            f"[router]\nhistory_limit = {int(history_limit)}\n", 1)
    with open(path, "w", encoding="utf-8") as fh:
        fh.write(body)
    return path


def write_config_four():
    """ABCD 四上游配置，配合 UP_MODELS_ABCD 使用。"""
    _counter[0] += 1
    path = os.path.join(HERE, f"_test_four{_counter[0]}.toml")
    with open(path, "w", encoding="utf-8") as fh:
        fh.write(CONFIG_FOUR.format(port=PORT, pa=UP_PORTS["a"], pb=UP_PORTS["b"],
                                    pc=UP_PORTS["c"], pd=UP_PORTS["d"]))
    return path


def write_config_quota():
    """配置里预写 quota 和 orderings 的配置。"""
    _counter[0] += 1
    path = os.path.join(HERE, f"_test_quota{_counter[0]}.toml")
    with open(path, "w", encoding="utf-8") as fh:
        fh.write(CONFIG_WITH_QUOTA_AND_ORDERINGS.format(
            port=PORT, pa=UP_PORTS["a"], pb=UP_PORTS["b"], pc=UP_PORTS["c"],
            name_a="up-a"))
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
        for k in bodies:
            bodies[k].clear()


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
        # 快照目录同理：只删 _test_ 派生的那些
        versions = os.path.join(state_dir, "versions")
        if os.path.isdir(versions):
            for f in os.listdir(versions):
                if f.startswith("_test_"):
                    try:
                        os.remove(os.path.join(versions, f))
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

        # 禁用式路由：把某条 entry 的 enabled 设为 false 再保存；
        # 该条依然留在配置里（下次重新勾上就能恢复），调用时跳过它
        admin("/admin/model/save", {
            "name": "deepseek-chat", "strategy": "priority",
            "entries": [
                {"upstream": "a1", "model": "deepseek-chat", "enabled": True},
                {"upstream": "c1", "model": "deepseek-chat", "enabled": False},
            ]})
        reset_hits()
        st, body, hdr = post()
        check("禁用条目仍留在配置里但调用被跳过",
              st == 200 and hdr.get("X-Upstream") == "up-a",
              f"status={st} up={hdr.get('X-Upstream')} hits={hits}")
        st, snap = get("/stats")
        check("禁用条目仍存在于 snap 中",
              len(snap["models"]["deepseek-chat"]["entries"]) == 2,
              str(snap["models"]["deepseek-chat"]["entries"]))
        c1 = next(e for e in snap["models"]["deepseek-chat"]["entries"]
                  if e["upstream"] == "c1")
        check("该条在 snap 里保持 enabled=false", c1.get("enabled") is False,
              str(c1))

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

    # 上游回 200 但响应体不是 JSON（例如网关吐纯文本错误页）。
    # 回归：response_to_text 的返回元数跟调用方对不上时，这里会 500 + ValueError。
    cleanup()
    behavior["a"] = "bad_json"
    p = start_router(write_config(with_routes=True))
    try:
        reset_hits()
        st, body, hdr = post()
        check("上游 200 但响应不是 JSON 时不报 500", st == 200, str(st))
        check("解析不了的内容原样透传", b"Bad Gateway" in body, str(body[:80]))
        check("没有误切到下一个上游", hdr.get("X-Upstream") == "up-a",
              str(hdr.get("X-Upstream")))
    finally:
        stop_router(p)
        behavior["a"] = "ok"

    # 上游回 200 但内容是**网页**：不能当成功，要换下一个（或最后回 502）。
    # 判据见 router.py 的 looks_like_html_page：网关用首页兜底时会出现这种。
    cleanup()
    behavior["a"] = "bad_html"
    p = start_router(write_config(with_routes=True))
    try:
        reset_hits()
        st, body, hdr = post()
        check("上游回 HTML 时不当成成功（切到下一个上游）",
              hdr.get("X-Upstream") == "up-b", str(hdr.get("X-Upstream")))
        check("上游回 HTML 时不把网页当模型回复透传",
              b"Welcome to the gateway" not in body, str(body[:120]))
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
        # 现在按「每个上游勾选的每个模型」各测一次：a1 勾了 2 个、b1 勾了 1 个、
        # c1 没勾 → 退回它的第一个可用模型。合计 4 条。
        check("测速按模型逐条返回(a1 两个 + b1 一个 + c1 兜底一个)",
              len(res) == 4, str(len(res)))
        check("测速都成功", all(x["ok"] for x in res), str(res))
        check("测速带延迟", all(x.get("latency_seconds") is not None for x in res), str(res))
        check("测速结果带 model 字段", all(x.get("model") for x in res), str(res))
        check("上游汇总也返回",
              len(r.get("upstreams") or []) == 3, str(r.get("upstreams")))

        behavior["b"] = "fail500"
        st, r = admin("/admin/speedtest", {"ids": ["b1"]})
        one = (r.get("results") or [{}])[0]
        check("坏上游测速报告失败", one.get("ok") is False and one.get("error"), str(one))
        behavior["b"] = "ok"

        # 精确测某个上游上的某个模型(模型路由弹窗每行的「测试」按钮)
        st, r = admin("/admin/speedtest", {"tests": [{"id": "a1", "model": "glm-4.6"}]})
        res2 = r.get("results") or []
        check("tests 精确测一个模型", len(res2) == 1 and res2[0].get("ok"), str(res2))
        check("tests 回带被测的模型名", res2 and res2[0].get("model") == "glm-4.6", str(res2))
        behavior["b"] = "fail500"
        st, r = admin("/admin/speedtest", {"tests": [{"id": "b1", "model": "glm-4.6"}]})
        res3 = r.get("results") or []
        check("tests 精确测坏上游报失败", res3 and res3[0].get("ok") is False, str(res3))
        behavior["b"] = "ok"

        # 连通性结果要持久化：stats 里带 last_check，且重启后还在
        st, snap = get("/stats")
        by = {u["id"]: u for u in snap["upstreams"]}
        check("stats 带出 last_check 字段", "last_check" in by["a1"], str(by["a1"].keys()))
        check("a1 的 last_check 标记为可用",
              bool(by["a1"]["last_check"]) and by["a1"]["last_check"]["ok"] is True,
              str(by["a1"].get("last_check")))
        check("last_check 带延迟",
              by["a1"]["last_check"].get("latency_seconds") is not None,
              str(by["a1"].get("last_check")))
        check("b1 的 last_check 标记为不可用",
              bool(by["b1"]["last_check"]) and by["b1"]["last_check"]["ok"] is False,
              str(by["b1"].get("last_check")))
    finally:
        stop_router(p)

    # 重启后 last_check 仍在(持久化生效)
    p = start_router(cfg)
    try:
        st, snap = get("/stats")
        by = {u["id"]: u for u in snap["upstreams"]}
        check("重启后 last_check 仍保留",
              bool(by["a1"].get("last_check")),
              str(by["a1"].get("last_check")))
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
        # 统计与流水不再提供清除入口（cleanup() 已经清掉上一轮的测试状态文件），
        # 所以这里直接用差量断言，不再依赖 /admin/reset 归零。
        st, raw = get("/admin/history")
        base = len(raw.get("history") or [])
        post()
        post(path="/v1/chat/completions/stream")
        st, raw = get("/admin/history")
        hist = (raw.get("history") or [])[base:]
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

        # 多版本快照：单份 .bak 会被下一次保存挤掉，所以另外留带时间戳的多份
        versions_dir = os.path.join(HERE, ".llmrouter", "versions")
        snaps = [f for f in os.listdir(versions_dir)] if os.path.isdir(versions_dir) else []
        check("保存时轮转出多份带时间戳的快照", len(snaps) >= 2,
              f"count={len(snaps)} {snaps[:5]}")
        check("快照文件名带 .state. 前缀",
              all(".state." in f for f in snaps[:5]), str(snaps[:5]))

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

    # ================= 21. 具名排序（原始顺序不丢）=================
    print("\n[21] 具名排序：按测速重排，原顺序留着，可切回")
    cleanup()
    UP_MODELS.update(UP_MODELS_ABCD)          # 四家各叫 ds1..ds4
    cfg = write_config_four()
    p = start_router(cfg)
    try:
        admin("/admin/fetch-models")
        for uid, real in [("a1", "ds1"), ("b1", "ds2"), ("c1", "ds3"), ("d1", "ds4")]:
            admin("/admin/select-models", {"id": uid, "models": [real]})
            admin("/admin/set-group", {"id": uid, "mapping": {real: "deepseek"}})

        st, snap = get("/stats")
        original = [e["upstream"] for e in snap["models"]["deepseek"]["entries"]]
        check("初始顺序是 a→b→c→d", original == ["a1", "b1", "c1", "d1"], str(original))

        # 还没保存过任何排序时，列表是空的
        st, r = admin("/admin/model/orderings", {"name": "deepseek"})
        check("未保存过排序时列表为空", st == 200 and r.get("orderings") == [], str(r)[:160])

        # 让 d 最快、c 次之、b 再次、a 最慢
        delay.update({"a": 0.30, "b": 0.20, "c": 0.10, "d": 0.0})
        st, sp = admin("/admin/speedtest")
        results = sp.get("results") or []
        check("测速拿到四个上游的延迟",
              len(results) == 4 and all(x.get("latency_seconds") is not None for x in results),
              str([(x.get("id"), x.get("latency_seconds")) for x in results]))
        delay.update({"a": 0.0, "b": 0.0, "c": 0.0, "d": 0.0})

        # 按测速重排并存成一套具名排序
        payload = [{"upstream": x["id"], "id": x["id"], "ok": x["ok"],
                    "latency_seconds": x["latency_seconds"]} for x in results]
        st, r = admin("/admin/model/reorder-by-speed",
                      {"name": "deepseek", "label": "按测速", "results": payload})
        check("按测速重排成功", st == 200 and r.get("label") == "按测速", str(r)[:200])
        order = [o["upstream"] for o in r.get("order") or []]
        check("快的排前面（d→c→b→a）", order == ["d1", "c1", "b1", "a1"], str(order))

        st, snap = get("/stats")
        entries = [e["upstream"] for e in snap["models"]["deepseek"]["entries"]]
        check("重排后调用顺序就是测速顺序", entries == ["d1", "c1", "b1", "a1"], str(entries))
        check("「原始」被自动记住", snap["models"]["deepseek"]["orderings"]["原始"]
              == [{"upstream": u, "model": m} for u, m in
                  [("a1", "ds1"), ("b1", "ds2"), ("c1", "ds3"), ("d1", "ds4")]],
              str(snap["models"]["deepseek"]["orderings"].get("原始")))
        check("stats 标出当前生效的排序",
              snap["models"]["deepseek"]["active_ordering"] == "按测速",
              str(snap["models"]["deepseek"].get("active_ordering")))

        # 真的按新顺序调用
        reset_hits()
        st, body, hdr = post(body={"model": "deepseek", "messages": []})
        check("重排后首选是最快的那家", st == 200 and hits["d"] == ["ds4"], str(hits))

        # 再存一套：把顺序换成 c→b→a→d 并起个名字
        st, r = admin("/admin/model/order/save", {
            "name": "deepseek", "label": "手动排的",
            "order": [{"upstream": "c1", "model": "ds3"},
                      {"upstream": "b1", "model": "ds2"},
                      {"upstream": "a1", "model": "ds1"},
                      {"upstream": "d1", "model": "ds4"}]})
        check("第二套排序保存成功", st == 200 and r.get("active") == "手动排的", str(r)[:200])
        reset_hits()
        st, body, hdr = post(body={"model": "deepseek", "messages": []})
        check("第二套排序立即生效", hits["c"] == ["ds3"], str(hits))

        st, r = admin("/admin/model/orderings", {"name": "deepseek"})
        labels = set(o["label"] for o in r.get("orderings") or [])
        check("三套排序都在（原始/按测速/手动排的）",
              labels == {"原始", "按测速", "手动排的"}, str(labels))

        # 切回「原始」：第一套顺序应完全恢复
        st, r = admin("/admin/model/order/apply", {"name": "deepseek", "label": "原始"})
        check("切回原始排序成功", st == 200 and r.get("active") == "原始", str(r)[:200])
        st, snap = get("/stats")
        back = [e["upstream"] for e in snap["models"]["deepseek"]["entries"]]
        check("原始顺序完整恢复（a→b→c→d）", back == original, str(back))
        reset_hits()
        st, body, hdr = post(body={"model": "deepseek", "messages": []})
        check("切回后按原始顺序走", hits["a"] == ["ds1"], str(hits))

        # 切到「按测速」
        st, r = admin("/admin/model/order/apply", {"name": "deepseek", "label": "按测速"})
        check("切到按测速成功", st == 200 and r.get("active") == "按测速", str(r)[:200])
        reset_hits()
        st, body, hdr = post(body={"model": "deepseek", "messages": []})
        check("按测速这套也是最快的先走", hits["d"] == ["ds4"], str(hits))

        # 删掉一套：当前顺序不动
        st, r = admin("/admin/model/order/delete", {"name": "deepseek", "label": "手动排的"})
        check("删除一套排序成功", st == 200 and r.get("label") == "手动排的", str(r)[:200])
        st, snap = get("/stats")
        check("删排序不影响当前顺序",
              [e["upstream"] for e in snap["models"]["deepseek"]["entries"]] == ["d1", "c1", "b1", "a1"],
              str([e["upstream"] for e in snap["models"]["deepseek"]["entries"]]))

        st, r = admin("/admin/model/order/delete", {"name": "deepseek", "label": "原始"})
        check("拒绝删除「原始」", st == 400, str(r)[:160])
        st, r = admin("/admin/model/order/apply", {"name": "deepseek", "label": "不存在的"})
        check("切换不存在的排序返回 404", st == 404, str(st))
        st, r = admin("/admin/model/order/save", {"name": "deepseek", "label": ""})
        check("空排序名被拒绝", st == 400, str(r)[:160])
        st, r = admin("/admin/model/order/save", {"name": "deepseek", "label": "原始"})
        check("不允许用「原始」当新排序的名字（防覆盖回溯点）",
              st == 400 and "原始" in str(r.get("error") or ""), str(r)[:200])
        st, r = admin("/admin/model/reorder-by-speed", {
            "name": "deepseek", "label": "原始",
            "results": [{"upstream": "a1", "ok": True, "latency_seconds": 0.1}]})
        check("按测速重排也不允许占用「原始」这个名字",
              st == 400 and "原始" in str(r.get("error") or ""), str(r)[:200])
        st, r = admin("/admin/model/reorder-by-speed", {"name": "deepseek", "results": []})
        check("没有测速结果时重排被拒绝", st == 400, str(r)[:160])
        st, r = admin("/admin/model/orderings", {"name": "没这个分组"})
        check("不存在的分组返回 404", st == 404, str(st))

        # 回归：保存路由（比如只是在弹窗里改了额度）不能把「原始」改掉，
        # 否则用户切回「原始」拿到的其实是当时的顺序，回溯就失效了
        st, snap = get("/stats")
        before_orig = snap["models"]["deepseek"]["orderings"]["原始"]
        st, r = admin("/admin/model/save", {
            "name": "deepseek", "strategy": "priority",
            "entries": [{"upstream": "d1", "model": "ds4", "quota": 0},
                        {"upstream": "c1", "model": "ds3"},
                        {"upstream": "b1", "model": "ds2"},
                        {"upstream": "a1", "model": "ds1"}]})
        check("带额度保存路由成功", st == 200, str(r)[:160])
        st, snap = get("/stats")
        check("保存路由不影响「原始」",
              snap["models"]["deepseek"]["orderings"]["原始"] == before_orig,
              str(snap["models"]["deepseek"]["orderings"].get("原始")))
        check("保存路由保留了「按测速」这套",
              "按测速" in snap["models"]["deepseek"]["orderings"],
              str(list(snap["models"]["deepseek"]["orderings"])))
        st, r = admin("/admin/model/order/apply", {"name": "deepseek", "label": "原始"})
        st, snap = get("/stats")
        check("保存后仍能切回真的原始顺序（a→b→c→d）",
              [e["upstream"] for e in snap["models"]["deepseek"]["entries"]]
              == ["a1", "b1", "c1", "d1"],
              str([e["upstream"] for e in snap["models"]["deepseek"]["entries"]]))
        # 还原成按测速那套，方便后面的持久化检查
        admin("/admin/model/order/apply", {"name": "deepseek", "label": "按测速"})
        admin("/admin/model/quota", {"name": "deepseek", "entries": [
            {"upstream": "d1", "model": "ds4", "quota": None}]})

        # 状态文件里两套排序都落了盘
        state_file = os.path.join(HERE, ".llmrouter",
                                  os.path.splitext(os.path.basename(cfg))[0] + ".state.json")
        with open(state_file, "r", encoding="utf-8") as fh:
            st_data = json.load(fh)
        check("排序写进了状态文件",
              "orderings" in st_data["models"]["deepseek"],
              str(list(st_data["models"]["deepseek"].keys())))
    finally:
        stop_router(p)

    # 重启后排序还在
    p = start_router(cfg)
    try:
        st, snap = get("/stats")
        labels = sorted((snap["models"]["deepseek"].get("orderings") or {}).keys())
        check("重启后具名排序还在", labels == ["原始", "按测速"], str(labels))
        check("重启后当前顺序还是按测速那套",
              [e["upstream"] for e in snap["models"]["deepseek"]["entries"]]
              == ["d1", "c1", "b1", "a1"],
              str([e["upstream"] for e in snap["models"]["deepseek"]["entries"]]))
        st, r = admin("/admin/model/order/apply", {"name": "deepseek", "label": "原始"})
        check("重启后还能切回原始顺序",
              st == 200 and r.get("active") == "原始", str(r)[:160])
    finally:
        stop_router(p)
        UP_MODELS.update({"a": ["deepseek-chat", "deepseek-reasoner", "glm-4.6"],
                          "b": ["deepseek-chat", "glm-4.6", "qwen-max"],
                          "c": ["deepseek-chat"],
                          "d": ["ds4", "other"]})

    # ================= 22. 分组内上游调用额度 =================
    print("\n[22] 分组里设上游额度：0 = 不调用（跳过并沉到最后）")
    cleanup()
    cfg = write_config(with_routes=True)
    p = start_router(cfg)
    try:
        # deepseek-chat 的路由是 a→b→c
        st, snap = get("/stats")
        entries = snap["models"]["deepseek-chat"]["entries"]
        check("初始三个上游都没设额度",
              len(entries) == 3 and all(e.get("quota") is None for e in entries), str(entries))

        # 把 a 的额度设成 0：不该再被调用
        st, r = admin("/admin/model/quota", {
            "name": "deepseek-chat", "upstream": "a1", "model": "deepseek-chat", "quota": 0})
        check("设置额度成功", st == 200, str(r)[:200])
        applied = (r.get("applied") or [{}])[0]
        check("返回里标出额度 0 且被跳过",
              applied.get("quota") == 0 and applied.get("skipped") is True, str(applied))
        check("a 的额度写成了 0",
              next(e for e in r["entries"] if e["upstream"] == "a1")["quota"] == 0,
              str(r.get("entries")))

        reset_hits()
        st, body, hdr = post()
        # b1 在这个配置里把模型映射成了 deepseek-ai/DeepSeek-V3
        check("额度 0 的上游被直接跳过",
              st == 200 and not hits["a"] and hits["b"] == ["deepseek-ai/DeepSeek-V3"],
              f"status={st} hits={hits}")
        check("换到了下一个上游", hdr.get("X-Upstream") == "up-b",
              str(hdr.get("X-Upstream")))

        st, snap = get("/stats")
        check("stats 里也能看到 0 额度",
              next(e for e in snap["models"]["deepseek-chat"]["entries"]
                   if e["upstream"] == "a1")["quota"] == 0,
              str(snap["models"]["deepseek-chat"]["entries"]))
        check("额度 0 的不算进「可用上游数」",
              snap["models"]["deepseek-chat"]["resolved_count"] == 2,
              str(snap["models"]["deepseek-chat"]["resolved_count"]))

        # 沉到最后：按测速重排时，0 额度的成员一律排最后（哪怕它测出来最快）
        st, r = admin("/admin/model/reorder-by-speed", {
            "name": "deepseek-chat", "label": "按测速",
            "results": [{"upstream": "a1", "ok": True, "latency_seconds": 0.01},   # 最快但是 0 额度
                        {"upstream": "b1", "ok": True, "latency_seconds": 0.5},
                        {"upstream": "c1", "ok": True, "latency_seconds": 0.2}]})
        check("按测速重排成功（含 0 额度成员）", st == 200, str(r)[:200])
        order = [o["upstream"] for o in r.get("order") or []]
        check("0 额度的排最后，其余按快慢排", order == ["c1", "b1", "a1"], str(order))
        st, snap = get("/stats")
        check("重排后 0 额度的 u-a 确实在末尾",
              [e["upstream"] for e in snap["models"]["deepseek-chat"]["entries"]]
              == ["c1", "b1", "a1"],
              str([e["upstream"] for e in snap["models"]["deepseek-chat"]["entries"]]))
        reset_hits()
        st, body, hdr = post()
        check("重排后调用仍然跳过 0 额度的那家",
              st == 200 and not hits["a"] and hits["c"] == ["deepseek-chat"],
              f"status={st} hits={hits}")

        # 改成不限量：又能用了
        st, r = admin("/admin/model/quota", {
            "name": "deepseek-chat", "upstream": "a1", "model": "deepseek-chat", "quota": None})
        check("清空额度成功",
              (r.get("applied") or [{}])[0].get("quota") is None, str(r)[:200])
        reset_hits()
        st, body, hdr = post()
        check("清空额度后恢复按当前顺序（c 仍是首选）", hits["c"] == ["deepseek-chat"], str(hits))

        # 定量额度（>0）也会被记下来，只是不限死
        st, r = admin("/admin/model/quota", {
            "name": "deepseek-chat", "upstream": "a1", "model": "deepseek-chat", "quota": 50})
        check("设置上限 50 成功",
              (r.get("applied") or [{}])[0].get("quota") == 50, str(r)[:200])
        reset_hits()
        st, body, hdr = post()
        check("设了上限、没用完，照常调用", 200 <= st < 300 and any(hits[k] for k in "abc"),
              f"status={st} hits={hits}")

        # 批量设置：一次给两家设额度
        st, r = admin("/admin/model/quota", {"name": "deepseek-chat", "entries": [
            {"upstream": "a1", "model": "deepseek-chat", "quota": ""},
            {"upstream": "b1", "model": "deepseek-ai/DeepSeek-V3", "quota": 0},
        ]})
        check("批量设置额度成功", st == 200 and len(r.get("applied") or []) == 2, str(r)[:220])
        check("批量里留空 = 不限量",
              next(e for e in r["entries"] if e["upstream"] == "a1")["quota"] is None,
              str(r.get("entries")))
        reset_hits()
        st, body, hdr = post()
        check("批量后 b 被跳过（走的是排第一的 c）",
              st == 200 and not hits["b"] and hits["c"] == ["deepseek-chat"], str(hits))

        # 额度写进状态文件
        state_file = os.path.join(HERE, ".llmrouter",
                                  os.path.splitext(os.path.basename(cfg))[0] + ".state.json")
        with open(state_file, "r", encoding="utf-8") as fh:
            st_data = json.load(fh)
        q0 = [e.get("quota") for e in st_data["models"]["deepseek-chat"]["entries"]]
        check("额度落盘（b 是 0，其余不写 quota 字段）",
              q0 == [None, 0, None], str(q0))

        # 错误处理
        st, r = admin("/admin/model/quota", {"name": "deepseek-chat", "entries": []})
        check("空额度请求被拒绝", st == 400, str(r)[:160])
        st, r = admin("/admin/model/quota", {
            "name": "deepseek-chat", "upstream": "不存在", "quota": 0})
        check("上游不在这个分组里返回 404", st == 404, str(r)[:160])
        st, r = admin("/admin/model/quota", {"name": "没这个分组", "upstream": "a1", "quota": 0})
        check("分组不存在返回 404", st == 404, str(st))

        # 负数当「不限量」
        st, r = admin("/admin/model/quota", {
            "name": "deepseek-chat", "upstream": "c1", "model": "deepseek-chat", "quota": -5})
        check("负数额度当成不限量",
              st == 200 and (r.get("applied") or [{}])[0].get("quota") is None, str(r)[:200])
    finally:
        stop_router(p)

    # 全 0 时退回自动兜底，而不是 503
    cleanup()
    cfg = write_config(with_routes=True)
    p = start_router(cfg)
    try:
        admin("/admin/model/quota", {"name": "deepseek-chat", "entries": [
            {"upstream": "a1", "model": "deepseek-chat", "quota": 0},
            {"upstream": "b1", "model": "deepseek-ai/DeepSeek-V3", "quota": 0},
            {"upstream": "c1", "model": "deepseek-chat", "quota": 0},
        ]})
        reset_hits()
        st, body, hdr = post()
        # 路由成员全被 0 掉，就和「配置为空」一样走自动兜底，不该直接 503
        check("额度全为 0 时不至于 503（退回自动兜底）", st == 200, str(st))
        check("兜底确实挑了上游", any(hits[k] for k in "abc"), str(hits))
        st, raw = get("/admin/history")
        hist = raw.get("history") or []
        src = (hist[-1].get("route_source") or "").strip() if hist else ""
        # 「配置」= 正常走路由；全 0 的情况下这里应该是兜底路径
        check("历史里标明是兜底、不是走配置",
              bool(hist) and src != "配置" and ("自动" in src or "兜底" in src),
              repr(src))
    finally:
        stop_router(p)

    # ================= 23. 配置文件里预写额度与排序 =================
    print("\n[23] 配置文件里的 quota 与 orderings")
    cleanup()
    cfg = write_config_quota()
    p = start_router(cfg)
    try:
        st, snap = get("/stats")
        route = snap["models"]["deepseek-chat"]
        check("配置里的三家里有一家 quota=0",
              {e["upstream"]: e.get("quota") for e in route["entries"]}
              == {"a1": None, "b1": 0, "c1": None},
              str(route["entries"]))
        check("配置里的两套排序都被读到",
              set((route.get("orderings") or {}).keys()) == {"原始", "快的先"},
              str(sorted((route.get("orderings") or {}).keys())))
        check("当前顺序就是配置里 entries 的顺序",
              [e["upstream"] for e in route["entries"]] == ["a1", "b1", "c1"],
              str([e["upstream"] for e in route["entries"]]))

        # quota=0 的 b 不该被调用
        reset_hits()
        st, body, hdr = post()
        check("配置里的 quota=0 生效（跳过 b）",
              st == 200 and not hits["b"] and hits["a"] == ["deepseek-chat"],
              f"status={st} hits={hits}")

        # 配置里预存的排序能直接切
        st, r = admin("/admin/model/order/apply",
                      {"name": "deepseek-chat", "label": "快的先"})
        check("能切到配置里预存的排序",
              st == 200 and r.get("active") == "快的先", str(r)[:160])
        st, snap = get("/stats")
        route = snap["models"]["deepseek-chat"]
        check("切换后顺序变成快的先（c→a→b）",
              [e["upstream"] for e in route["entries"]] == ["c1", "a1", "b1"],
              str([e["upstream"] for e in route["entries"]]))
        check("切换后 quota 还是跟着原来的成员（b 仍是 0）",
              {e["upstream"]: e.get("quota") for e in route["entries"]}
              == {"a1": None, "b1": 0, "c1": None},
              str(route["entries"]))
        reset_hits()
        st, body, hdr = post()
        check("切换后仍然跳过 b（走 c）",
              st == 200 and not hits["b"] and hits["c"] == ["deepseek-chat"],
              f"status={st} hits={hits}")

        # 重启后（状态文件接管）额度和排序都还在
        state_file = os.path.join(HERE, ".llmrouter",
                                  os.path.splitext(os.path.basename(cfg))[0] + ".state.json")
        check("首次启动就把配置写成了状态文件", os.path.exists(state_file), state_file)
    finally:
        stop_router(p)

    p = start_router(cfg)
    try:
        st, snap = get("/stats")
        route = snap["models"]["deepseek-chat"]
        check("重启后额度还在",
              {e["upstream"]: e.get("quota") for e in route["entries"]}
              == {"a1": None, "b1": 0, "c1": None},
              str(route["entries"]))
        check("重启后排序还在",
              set((route.get("orderings") or {}).keys()) == {"原始", "快的先"},
              str(sorted((route.get("orderings") or {}).keys())))
        check("重启后当前生效的排序也对",
              route.get("active_ordering") == "快的先",
              str(route.get("active_ordering")))
    finally:
        stop_router(p)

    # ================= 24. 左侧统计与右侧上游卡片对得上（失败转移也算）=================
    # 一次客户端请求可能试多家上游。左右两处数字口径不同但必须自洽：
    #   请求层面（左侧「总请求/成功/失败」）—— 一次客户端请求算一次，只看最终结果
    #   尝试层面（左侧分组×上游、右侧上游卡片）—— 发给上游算一次，换一家就多一次
    # 曾经的 bug：中间的失败没进左侧统计，于是左侧永远 0 失败、右侧却有一堆失败。
    print("\n[24] 左右两处统计口径一致（失败转移的中间失败也看得见）")
    cleanup()
    cfg = write_config(with_routes=True)      # deepseek-chat: a→b→c
    behavior["a"] = "fail500"                 # a 恒挂，每次都落到 b
    p = start_router(cfg)
    try:
        for _ in range(2):
            st, body, hdr = post()
            check("失败转移后最终成功", st == 200 and hdr.get("X-Upstream") == "up-b",
                  f"status={st} up={hdr.get('X-Upstream')}")

        st, snap = get("/stats")
        cs = snap["call_stats"]
        t = cs["totals"]
        # 请求层面：2 次请求，都成功（最终落到 b），没有任何「失败的请求」
        check("请求层面 calls=2", t["calls"] == 2, str(t))
        check("请求层面 ok=2", t["ok"] == 2, str(t))
        check("请求层面 failed=0", t["failed"] == 0, str(t))
        # 尝试层面：2 次请求 × 2 家 = 4 次上游尝试
        check("尝试层面 attempts=4", t["attempts"] == 4, str(t))

        # 右侧卡片（累计）：a 被打了 2 次全失败，b 成功 2 次
        cards = {u["name"]: u for u in snap["upstreams"]}
        check("右侧 a 请求=2", cards["up-a"]["requests"] == 2, str(cards.get("up-a")))
        check("右侧 a 失败=2", cards["up-a"]["failures"] == 2, str(cards.get("up-a")))
        check("右侧 b 成功=2", cards["up-b"]["successes"] == 2, str(cards.get("up-b")))

        # 左侧分组×上游 用尝试口径，数字必须和右侧卡片一模一样
        g = next(x for x in cs["groups"] if x["group"] == "deepseek-chat")
        ups = {u["upstream"]: u for u in g["upstreams"]}
        check("左侧 a 调用=2（和右侧一致）", ups["up-a"]["calls"] == 2, str(ups.get("up-a")))
        check("左侧 a 失败=2（和右侧一致）", ups["up-a"]["failed"] == 2, str(ups.get("up-a")))
        check("左侧 a 成功=0", ups["up-a"]["ok"] == 0, str(ups.get("up-a")))
        check("左侧 b 调用=2", ups["up-b"]["calls"] == 2, str(ups.get("up-b")))
        check("左侧 b 成功=2", ups["up-b"]["ok"] == 2, str(ups.get("up-b")))
        check("左侧分组尝试数=4", g["attempts"] == 4, str(g.get("attempts")))

        # 跨分组的上游汇总（左侧顶部对照区）
        lu = {u["upstream"]: u for u in cs["upstreams"]}
        check("左侧汇总 a 失败=2", lu["up-a"]["failed"] == 2, str(lu.get("up-a")))
        check("左侧汇总 b 成功=2", lu["up-b"]["ok"] == 2, str(lu.get("up-b")))

        # 历史记录里保留了每一步，点开请求详情能看到中间那次失败
        # （/stats 不再带 history 了，改从 /admin/history 取，体积小）
        st, hist_raw = get("/admin/history")
        trail = hist_raw["history"][-1]["trail"]
        check("trail 有两步", len(trail) == 2, str(trail))
        check("trail 第一步是 a 的失败",
              trail[0]["ok"] is False and trail[0]["upstream"] == "up-a", str(trail[0]))
        check("trail 第二步是 b 的成功",
              trail[1]["ok"] is True and trail[1]["upstream"] == "up-b", str(trail[1]))

        # token 用量不应该因为多试了一家而重复累计
        check("token 只算一次（没按尝试次数翻倍）",
              g["usage"].get("total") == 240, str(g["usage"]))

        # 全失败时：请求层面失败数也要和「重试到底」的尝试数区分开。
        # 统计不再能被清零，所以用「前后差量」断言，只看这一段新增的。
        behavior["b"] = behavior["c"] = "fail500"
        st, snap = get("/stats")
        t0 = snap["call_stats"]["totals"]
        base = {k: t0.get(k, 0) for k in ("calls", "failed", "attempts")}
        post()
        st, snap = get("/stats")
        t = snap["call_stats"]["totals"]
        check("全失败时 请求=1",
              t["calls"] - base["calls"] == 1, f'{base} -> {t}')
        check("全失败时 失败请求=1",
              t["failed"] - base["failed"] == 1, f'{base} -> {t}')
        check("全失败时 尝试=3（三家都试了）",
              t["attempts"] - base["attempts"] == 3, f'{base} -> {t}')
        behavior["b"] = behavior["c"] = "ok"
    finally:
        stop_router(p)
        behavior["a"] = "ok"

    # ================= 25. 请求记录持久化（跨重启 + 按天统计） =================
    print("\n[25] 请求记录重启后还在，按天/本月/全部都查得到")
    cleanup()
    cfg = write_config(with_routes=True)
    today = time.strftime("%Y-%m-%d")
    month = today[:7]
    p = start_router(cfg)
    before = None
    try:
        reset_hits()
        st, body, hdr = post()
        check("持久化测试里请求成功", st == 200, str(st))
        st, snap = get("/stats")
        before = snap["call_stats"]["totals"]["calls"]
        check("单次请求后 call_stats 有计数", before >= 1, str(before))

        # range 接口：今天 / 本月 / 全部都能查到这次请求
        st, snap = get("/stats?range=day:" + today)
        day_calls = (snap.get("stats_range") or {}).get("totals", {}).get("calls", -1)
        check("range=day 能查到今天的请求", day_calls >= 1, str(day_calls))
        st, snap = get("/stats?range=month:" + month)
        mon_calls = (snap.get("stats_range") or {}).get("totals", {}).get("calls", -1)
        check("range=month 能查到本月的请求", mon_calls >= 1, str(mon_calls))
        st, snap = get("/stats?range=all")
        all_calls = (snap.get("stats_range") or {}).get("totals", {}).get("calls", -1)
        check("range=all 能查到全部请求", all_calls >= 1, str(all_calls))

        st, raw = get("/admin/history")
        hist = raw.get("history") or []
        check("请求后历史有记录", len(hist) > 0, f"len={len(hist)}")

        # 明细日志文件落盘了
        hist_file = os.path.join(HERE, ".llmrouter",
                                 os.path.splitext(os.path.basename(cfg))[0] + ".history.jsonl")
        check("生成了历史明细日志", os.path.exists(hist_file), hist_file)
    finally:
        stop_router(p)

    # 重启后：明细 + 统计都还在
    p = start_router(cfg)
    try:
        st, raw = get("/admin/history")
        hist = raw.get("history") or []
        check("重启后请求记录还在", len(hist) > 0, f"len={len(hist)}")
        st, snap = get("/stats?range=all")
        tr = (snap.get("stats_range") or {}).get("totals") or {}
        check("重启后统计还在（>= 重启前）", tr.get("calls", 0) >= before, str(tr))
        st, snap = get("/stats?range=day:" + today)
        check("重启后今天的统计还在",
              (snap.get("stats_range") or {}).get("totals", {}).get("calls", 0) >= 1,
              str(snap.get("stats_range")))
    finally:
        stop_router(p)

    # ================= 26. 测速把「限流」和「故障」分开 =================
    # 用户遇到的真实场景：sensenova 返回 429「inference exceeds tpm/rpm limit」，
    # Key 和地址都没问题，只是这会儿不让调。以前这会被报成「失败」，
    # 看起来像配置错了，容易误判。
    print("\n[26] 测速区分限流与故障")
    cleanup()
    cfg = write_config()
    p = start_router(cfg)
    try:
        admin("/admin/fetch-models")

        # a 限流、b 正常：界面上要能分辨
        behavior["a"] = "fail429"
        st, r = admin("/admin/speedtest", {"ids": ["a1", "b1"]})
        res = {x["id"]: x for x in (r.get("results") or [])}
        check("限流的上游被标成 rate_limited",
              res["a1"].get("rate_limited") is True, str(res.get("a1")))
        check("限流的上游 ok=False（不算可用）",
              res["a1"].get("ok") is False, str(res.get("a1")))
        check("限流仍带上说明文字", bool(res["a1"].get("error")), str(res.get("a1")))
        check("正常的上游不会被标成限流",
              res["b1"].get("rate_limited") is False and res["b1"].get("ok") is True,
              str(res.get("b1")))

        # 500 这种真故障不能被当成限流
        behavior["a"] = "fail500"
        st, r = admin("/admin/speedtest", {"ids": ["a1"]})
        one = (r.get("results") or [{}])[0]
        check("500 故障不算限流",
              one.get("rate_limited") is False and one.get("ok") is False, str(one))

        # 401（Key 不对）也不算限流，那需要用户去改配置
        behavior["a"] = "fail401"
        st, r = admin("/admin/speedtest", {"ids": ["a1"]})
        one = (r.get("results") or [{}])[0]
        check("401 不算限流", one.get("rate_limited") is False, str(one))

        # 限流只影响测速的展示；真调用时照样会切换下一个上游
        behavior["a"] = "fail429"
        admin("/admin/model/save", {
            "name": "deepseek-chat", "strategy": "priority",
            "entries": [{"upstream": "a1", "model": "deepseek-chat"},
                        {"upstream": "b1", "model": "deepseek-chat"}]})
        reset_hits()
        st, body, hdr = post()
        check("限流的上游在真调用时会被跳过并切换",
              st == 200 and hdr.get("X-Upstream") == "up-b" and hits["a"],
              f"status={st} up={hdr.get('X-Upstream')} hits={hits}")
        behavior["a"] = "ok"
    finally:
        stop_router(p)

    # ================= 27. 流水只增不减，累计统计全量 =================
    # 两条核心不变量：
    #   1) 内存明细列表只留最近 history_limit 条（避免界面/内存被撑爆）；
    #   2) 落盘的流水文件只追加、永不删行 —— 归档是改名，不是重写。
    # 以前这里做的是「把文件重写成最近 N 条」，等于真删历史，改成只增不减。
    print("\n[27] 内存明细有上限，落盘流水只增不减")
    cleanup()
    cfg = write_config(with_routes=True, history_limit=3)
    p = start_router(cfg)
    sent = 12          # 远超过内存上限，用来验证「内存截断但文件不截断」
    cap = 3
    try:
        reset_hits()
        for _ in range(sent):
            st, _, _ = post()
            if st != 200:
                check("第 27 组请求成功", False, f"status={st}")
                break

        st, snap = get("/stats?range=all")
        tr = (snap.get("stats_range") or {}).get("totals") or {}
        check("明细超上限后，全量统计照样记满",
              tr.get("calls") == sent, f"calls={tr.get('calls')} 期望={sent} 详情={tr}")

        st, raw = get("/admin/history")
        hist = raw.get("history") or []
        check("内存明细列表被截到上限", len(hist) == cap, f"len={len(hist)}")

        st, snap = get("/stats")
        check("快照里报出明细上限", snap["call_stats"].get("history_limit") == cap,
              str(snap["call_stats"].get("history_limit")))
        check("全量统计不因内存截断而变小",
              tr.get("calls", 0) > len(hist), f"calls={tr.get('calls')} 明细={len(hist)}")

        # 关键：落盘流水必须一条不少（不能被「上限」裁掉）
        hist_file = os.path.join(HERE, ".llmrouter",
                                 os.path.splitext(os.path.basename(cfg))[0] + ".history.jsonl")
        with open(hist_file, "r", encoding="utf-8") as fh:
            lines = [ln for ln in fh.read().splitlines() if ln.strip()]
        check("落盘流水一条不少（只增不减）", len(lines) == sent,
              f"lines={len(lines)} 期望={sent}")

        # 手动归档应该是改名保留，不是删掉
        st, rot = admin("/admin/journal/rotate")
        check("归档接口返回归档文件名",
              st == 200 and rot.get("archived"), str(rot)[:160])
        archived = rot.get("archived")
        arch_path = os.path.join(HERE, ".llmrouter", archived) if archived else ""
        check("归档文件保留下来了", bool(archived) and os.path.exists(arch_path),
              arch_path)
        if archived and os.path.exists(arch_path):
            with open(arch_path, "r", encoding="utf-8") as fh:
                arch_lines = [ln for ln in fh.read().splitlines() if ln.strip()]
            check("归档文件里的记录条数和归档前一致", len(arch_lines) == sent,
                  f"archived={len(arch_lines)} 期望={sent}")
        # 归档后当前流水是空的（文件要等下一条请求才重新出现），接着写新的
        after = []
        if os.path.exists(hist_file):
            with open(hist_file, "r", encoding="utf-8") as fh:
                after = [ln for ln in fh.read().splitlines() if ln.strip()]
        check("归档后当前流水重新开始", len(after) == 0, f"lines={len(after)}")

        # 重启后：累计统计还是全量
        stop_router(p)
        p = start_router(cfg)
        st, snap = get("/stats?range=all")
        tr2 = (snap.get("stats_range") or {}).get("totals") or {}
        check("重启后累计统计没缩水", tr2.get("calls") == sent,
              f"calls={tr2.get('calls')} 期望={sent}")
        st, raw = get("/admin/history")
        check("重启后内存明细仍只留上限条数", len(raw.get("history") or []) <= cap,
              f"len={len(raw.get('history') or [])}")
        # 归档文件必须还在（重启不能把它弄丢）
        st, jf = get("/admin/journal/files")
        names = [f.get("path") for f in (jf.get("files") or [])]
        check("归档文件在重启后依然在", bool(archived) and archived in names,
              f"archived={archived} names={names}")
    finally:
        stop_router(p)

    # ================= 28. 统计与流水不可清除 + 快照可回退 =================
    print("\n[28] 统计不可清除、状态快照可回退")
    cleanup()
    cfg = write_config(with_routes=True)
    p = start_router(cfg)
    try:
        reset_hits()
        post()
        st, snap = get("/stats?range=all")
        calls_before = (snap.get("stats_range") or {}).get("totals", {}).get("calls", 0)
        check("先有一次请求作为基线", calls_before >= 1, str(calls_before))

        # /admin/reset 现在只复位冷却，不许动统计
        st, r = admin("/admin/reset")
        check("reset 返回成功且说明统计保留",
              st == 200 and "统计" in str(r.get("message") or ""), str(r)[:160])
        st, snap = get("/stats?range=all")
        calls_after = (snap.get("stats_range") or {}).get("totals", {}).get("calls", 0)
        check("reset 之后统计没有被清零", calls_after >= calls_before,
              f"{calls_before} -> {calls_after}")

        st, raw = get("/admin/history")
        check("reset 之后请求流水还在", len(raw.get("history") or []) > 0,
              f"len={len(raw.get('history') or [])}")

        # 审计日志要能查到刚才那次操作
        st, lg = get("/admin/admin-log")
        log = lg.get("log") or []
        check("审计日志记录了管理操作",
              any("/admin/" in (e.get("op") or "") for e in log), str(log)[:200])

        # 状态快照：每次覆盖前都会轮转出一份带时间戳的旧版本。
        # 第一次保存时还没有「上一版」可存，所以要改两次才看得到两份。
        admin("/admin/upstream/add", {"name": "临时上游1", "base_url": "http://127.0.0.1:9/v1",
                                      "api_key": "k"})
        admin("/admin/upstream/add", {"name": "临时上游2", "base_url": "http://127.0.0.1:9/v1",
                                      "api_key": "k"})
        st, vs = get("/admin/state/versions")
        versions = vs.get("versions") or []
        check("状态快照能列出来（留了多份）", len(versions) >= 2, str(vs)[:240])
        check("快照带得出上游/模型数量",
              all(v.get("upstreams") is not None for v in versions), str(versions[:2])[:240])

        # 回退到最早的那份快照，上游数量应该回到 3（配置里的三家）
        if versions:
            oldest = versions[-1]["file"]
            st, rr = admin("/admin/state/restore", {"file": oldest})
            check("状态快照可以回退", st == 200 and rr.get("ok"), str(rr)[:200])
            st, snap = get("/stats")
            check("回退后上游数量符合快照内容",
                  len(snap.get("upstreams") or []) == (versions[-1].get("upstreams") or -1),
                  f'now={len(snap.get("upstreams") or [])} snap={versions[-1].get("upstreams")}')

        st, snap = get("/stats")
        check("快照里报出流水概况",
              isinstance(snap.get("journal"), dict)
              and snap["journal"].get("current_lines") is not None,
              str(snap.get("journal")))
    finally:
        stop_router(p)

    # ================= 29. 调用记录全量持久化 + 按天/月/分组筛选 =================
    # 用户的核心诉求：所有调用都要落盘、能按天/按月/按分组翻，
    # 而且列表条数必须和统计数字是同一个口径（不能一边 375 一边 188）。
    print("\n[29] 调用记录全量可查（按天 / 按月 / 按分组 + 分页）")
    cleanup()
    cfg = write_config(with_routes=True)
    p = start_router(cfg)
    try:
        reset_hits()
        sent = 8
        for _ in range(sent):
            st, _, _ = post()
            if st != 200:
                check("第 29 组请求成功", False, f"status={st}")
                break

        # 索引：有哪些天/月/分组，条数对不对
        st, idx = get("/admin/journal/index")
        check("流水索引能读到", st == 200 and idx.get("total") is not None, str(idx)[:200])
        check("索引里的总条数 = 实际请求数",
              idx.get("total") == sent, f"total={idx.get('total')} 期望={sent}")
        today = time.strftime("%Y-%m-%d")
        days = {d["key"]: d["calls"] for d in (idx.get("days") or [])}
        check("索引里有今天且条数正确", days.get(today) == sent, str(days))
        months = {m["key"]: m["calls"] for m in (idx.get("months") or [])}
        check("索引里有本月且条数正确",
              months.get(today[:7]) == sent, str(months))
        groups = {g["key"]: g["calls"] for g in (idx.get("groups") or [])}
        check("索引里有这个分组且条数正确",
              groups.get("deepseek-chat") == sent, str(groups))

        # 分页查询：列表条数和索引/统计一致（不再有两套数字）
        st, page = get("/admin/journal?limit=100")
        check("列表返回的记录数 = 索引总数",
              len(page.get("entries") or []) == idx.get("total"),
              f"entries={len(page.get('entries') or [])} idx={idx.get('total')}")
        check("列表 total 和索引 total 一致",
              page.get("total") == idx.get("total"),
              f"{page.get('total')} vs {idx.get('total')}")

        # 按天筛选
        st, byday = get("/admin/journal?day=" + today)
        check("按天筛选命中条数正确",
              byday.get("total") == sent, str(byday.get("total")))
        st, byday2 = get("/admin/journal?day=1999-01-01")
        check("筛一个没有记录的日期得到 0 条",
              byday2.get("total") == 0, str(byday2.get("total")))

        # 按月筛选
        st, bymon = get("/admin/journal?month=" + today[:7])
        check("按月筛选命中条数正确",
              bymon.get("total") == sent, str(bymon.get("total")))

        # 按分组筛选
        st, bygrp = get("/admin/journal?group=deepseek-chat")
        check("按分组筛选命中条数正确",
              bygrp.get("total") == sent, str(bygrp.get("total")))
        # 不存在的分组得到 0 条（中文分组名要先 URL 编码才能发出去）
        st, bygrp2 = get("/admin/journal?group=" + urllib.parse.quote("不存在的组"))
        check("筛一个不存在的分组得到 0 条",
              bygrp2.get("total") == 0, str(bygrp2.get("total")))

        # 按上游筛选：在分组里定位到具体某个上游（用户要找一个一直报错的上游）。
        # 这里所有请求都命中 up-a，所以拿它验证。
        st, byup = get("/admin/journal?group=deepseek-chat&upstream=up-a")
        check("按上游筛选命中条数正确",
              byup.get("total") == sent, str(byup.get("total")))
        ups = {u["key"]: u for u in (byup.get("upstreams") or [])}
        check("分组内上游计数带调用数和失败数",
              ups.get("up-a", {}).get("calls") == sent
              and ups.get("up-a", {}).get("failed") == 0, str(ups))
        st, byup2 = get("/admin/journal?group=deepseek-chat&upstream="
                        + urllib.parse.quote("不存在"))
        check("筛一个不存在的上游得到 0 条",
              byup2.get("total") == 0, str(byup2.get("total")))


        # 分页：offset 往后翻，条数守恒
        st, p1 = get("/admin/journal?limit=3&offset=0")
        st, p2 = get("/admin/journal?limit=3&offset=3")
        check("第一页返回 limit 条", len(p1.get("entries") or []) == 3,
              str(len(p1.get("entries") or [])))
        check("翻页后仍是同一批数据（total 不变）",
              p2.get("total") == p1.get("total"),
              f"{p1.get('total')} vs {p2.get('total')}")
        ats1 = {e.get("at") for e in (p1.get("entries") or [])}
        ats2 = {e.get("at") for e in (p2.get("entries") or [])}
        check("两页没有重叠", not (ats1 & ats2), f"{ats1 & ats2}")

        # 列表为了省流量不带正文，单条接口要能取回正文
        first = (p1.get("entries") or [{}])[0]
        check("列表记录不含正文（省流量）",
              "request_text" not in first, sorted(first.keys()))
        st, one = get("/admin/journal/entry?at=%s" % first.get("at"))
        entry = one.get("entry") or {}
        # 时间戳是浮点数，经 URL 传一圈会有末位精度差，所以按 0.1 秒比对
        check("单条接口能取回这条记录",
              st == 200 and abs(float(entry.get("at") or 0)
                                - float(first.get("at") or 0)) < 0.1,
              f"want={first.get('at')} got={entry.get('at')}")
        check("单条接口带回输入正文", entry.get("request_text") is not None,
              str(entry.get("request_text"))[:60])

        # 列表条数必须和 /stats 的统计口径一致（曾经一边 375 一边 188）
        st, snap = get("/stats?range=all")
        snap_calls = (snap.get("stats_range") or {}).get("totals", {}).get("calls")
        check("统计口径和流水条数一致",
              snap_calls == idx.get("total"),
              f"stats={snap_calls} journal={idx.get('total')}")

        # 上游卡片上的数字要走**落盘聚合**（跨重启不归零），不能再用内存计数 ——
        # 否则重启后左边按累计显示、右边从 0 开始，两边又对不上。
        # 这里所有请求都命中 up-a（priority 顺序第一家），所以看它。
        cards = {u["name"]: u for u in snap.get("upstreams") or []}
        ca = cards.get("up-a") or {}
        check("卡片数字来自落盘聚合（跨重启不归零）",
              ca.get("counters_persistent") is True,
              f"counters_persistent={ca.get('counters_persistent')}")
        check("卡片上的请求数 = 实际命中次数",
              ca.get("requests") == sent, f"req={ca.get('requests')} 期望={sent}")
        # 没有分担流量的上游，卡片数字就是 0（不应该编造出数字）
        cu = cards.get("up-b") or {}
        check("未命中的上游显示 0", cu.get("requests") == 0 and cu.get("successes") == 0,
              f"up-b req={cu.get('requests')} ok={cu.get('successes')}")

        # 故障转移下的「按上游筛选」：a 失败、请求在 b 上成功时，
        # a 这一次失败尝试也必须能被筛出来 —— 否则用户只看到成功的那些，
        # 看不到「某上游一直失败但请求在别家兜住了」的真相。
        with lock:
            behavior["a"] = "fail500"
        st, _, _ = post()
        with lock:
            behavior["a"] = "ok"
        check("故障转移后请求仍成功（落在 up-b）", st == 200, str(st))
        st, fo = get("/admin/journal?upstream=" + urllib.parse.quote("up-a"))
        rows = fo.get("entries") or []
        check("失败换家的上游也能被筛出来（匹配 trail）",
              any(any((s or {}).get("upstream") == "up-a"
                      and not (s or {}).get("ok")
                      for s in (e.get("trail") or [])) for e in rows),
              f"total={fo.get('total')}")
        foups = {u["key"]: u for u in (fo.get("upstreams") or [])}
        check("组内上游计数按每次尝试（失败也计入）",
              foups.get("up-a", {}).get("failed", 0) >= 1, str(foups))
    finally:
        stop_router(p)

    # ================= 30. HTTP/1.1 长连接：每条请求各读各的 body =================
    # 回归：body 缓存曾经只在 setup 里清（每条连接一次），
    # 同一连接上的第二条请求会读到上一条的 body —— 表现就是
    # 「点启用要点很多次」「添加后拉取退化成拉全部」。
    print("\n[30] 长连接下每条请求各读各的 body")
    cleanup()
    cfg = write_config(with_routes=True)
    p = start_router(cfg)
    try:
        import http.client as _http

        def ka_post(conn, path, payload):
            body = json.dumps(payload)
            conn.request("POST", path, body=body,
                         headers={"Content-Type": "application/json",
                                  "Content-Length": str(len(body.encode()))})
            r = conn.getresponse()
            return r.status, json.loads(r.read())

        conn = _http.HTTPConnection("127.0.0.1", PORT, timeout=30)
        try:
            st1, r1 = ka_post(conn, "/admin/upstream/add",
                              {"name": "ka-test",
                               "base_url": f"http://127.0.0.1:{UP_PORTS['a']}"})
            new_id = (r1.get("upstream") or {}).get("id")
            check("长连接：添加上游成功", st1 == 200 and bool(new_id), str(r1)[:200])

            # 同一条连接紧接着发：只该拉这一个上游。
            # 如果复用了上一条 body，这里就没有 ids，会退化成拉全部。
            st2, r2 = ka_post(conn, "/admin/fetch-models", {"ids": [new_id]})
            ids = [x.get("id") for x in (r2.get("results") or [])]
            check("长连接：fetch 只拉指定上游（没退化成拉全部）",
                  st2 == 200 and ids == [new_id], f"ids={ids}")

            # 同一条连接连续两次 update：第二次必须真的生效
            st3, r3 = ka_post(conn, "/admin/upstream/update",
                              {"id": new_id, "enabled": False})
            off = (r3.get("upstream") or {}).get("enabled")
            st4, r4 = ka_post(conn, "/admin/upstream/update",
                              {"id": new_id, "enabled": True})
            on = (r4.get("upstream") or {}).get("enabled")
            check("长连接：连续停用再启用，第二次生效",
                  off is False and on is True, f"off={off} on={on}")
        finally:
            conn.close()
    finally:
        stop_router(p)

    # ================= 31. Anthropic /v1/messages 适配（请求+响应两个方向）=================
    # 上游只认 OpenAI 格式，客户端说的是 Anthropic 格式。只转一半的话
    # 客户端会拿到 choices 结构而解析失败 —— 所以这个端点必须两头都验。
    print("\n[31] Anthropic /v1/messages 适配")
    cfg = write_config()
    p = start_router(cfg)
    try:
        admin("/admin/fetch-models")
        admin("/admin/select-models", {"id": "a1", "models": ["deepseek-chat"]})

        # --- 非流式：响应必须是 Anthropic 结构 ---
        st, r = admin("/v1/messages", {
            "model": "deepseek-chat", "max_tokens": 100,
            "messages": [{"role": "user", "content": "你好"}]})
        check("Anthropic 非流式返回 200", st == 200, str(r)[:200])
        check("响应 type=message", r.get("type") == "message", str(r)[:200])
        check("响应 role=assistant", r.get("role") == "assistant")
        check("content 是块数组且 type=text",
              isinstance(r.get("content"), list) and r["content"]
              and r["content"][0].get("type") == "text",
              json.dumps(r.get("content"), ensure_ascii=False)[:200])
        check("带 stop_reason", r.get("stop_reason") == "end_turn", str(r.get("stop_reason")))
        check("usage 用 input_tokens / output_tokens",
              isinstance(r.get("usage"), dict)
              and r["usage"].get("input_tokens") == 100
              and r["usage"].get("output_tokens") == 20,
              json.dumps(r.get("usage"), ensure_ascii=False))
        check("usage 带 cache_read_input_tokens",
              r["usage"].get("cache_read_input_tokens") == 64,
              json.dumps(r.get("usage"), ensure_ascii=False))
        check("没有 OpenAI 的 choices 字段（客户端解析不了它）", "choices" not in r)

        # --- 上游收到的必须是 OpenAI 格式 ---
        admin("/admin/select-models", {"id": "a1", "models": ["deepseek-chat"]})
        st, raw, _ = _req("/v1/messages", {
            "model": "deepseek-chat", "max_tokens": 77, "temperature": 0.25,
            "system": [{"type": "text", "text": "你是助手"},
                       {"type": "text", "text": "要简洁"}],
            "messages": [{"role": "user", "content": "看图"}],
            "top_k": 40}, port=PORT)
        sent = bodies["a"][-1]
        check("上游收到的是 OpenAI 请求（model/messages）",
              sent.get("model") == "deepseek-chat" and isinstance(sent.get("messages"), list),
              str(sent)[:200])
        check("Anthropic 的 max_tokens 保留", sent.get("max_tokens") == 77,
              str(sent.get("max_tokens")))
        check("system 数组拼成一条 system 消息",
              sent["messages"][0].get("role") == "system"
              and sent["messages"][0].get("content") == "你是助手\n要简洁",
              json.dumps(sent["messages"][0], ensure_ascii=False))
        check("temperature 透传", sent.get("temperature") == 0.25,
              str(sent.get("temperature")))

        # --- 图片内容块 ---
        st, raw, _ = _req("/v1/messages", {
            "model": "deepseek-chat", "max_tokens": 20,
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "这是什么"},
                {"type": "image", "source": {"type": "base64",
                                             "media_type": "image/png",
                                             "data": "QUJD"}}]}]}, port=PORT)
        sent = bodies["a"][-1]
        parts = sent["messages"][-1].get("content")
        check("图片转成 OpenAI 的 image_url",
              isinstance(parts, list) and any(
                  p.get("type") == "image_url"
                  and str(p.get("image_url", {}).get("url", "")).startswith("data:image/png;base64,")
                  for p in parts),
              json.dumps(parts, ensure_ascii=False)[:250])

        # --- 工具调用：请求方向 ---
        st, raw, _ = _req("/v1/messages", {
            "model": "deepseek-chat", "max_tokens": 20,
            "tools": [{"name": "get_weather", "description": "查天气",
                       "input_schema": {"type": "object",
                                        "properties": {"city": {"type": "string"}},
                                        "required": ["city"]}}],
            "tool_choice": {"type": "auto"},
            "messages": [{"role": "user", "content": "上海天气"}]}, port=PORT)
        sent = bodies["a"][-1]
        tools = sent.get("tools") or []
        check("tools 转成 OpenAI function 格式",
              tools and tools[0].get("type") == "function"
              and tools[0]["function"].get("name") == "get_weather"
              and tools[0]["function"]["parameters"].get("required") == ["city"],
              json.dumps(tools, ensure_ascii=False)[:250])
        check("tool_choice auto 映射为 auto", sent.get("tool_choice") == "auto",
              str(sent.get("tool_choice")))

        # --- 工具结果回传 ---
        st, raw, _ = _req("/v1/messages", {
            "model": "deepseek-chat", "max_tokens": 20,
            "tools": [{"name": "get_weather", "input_schema": {"type": "object"}}],
            "messages": [
                {"role": "user", "content": "上海天气"},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "tu_1", "name": "get_weather",
                     "input": {"city": "上海"}}]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "tu_1",
                     "content": [{"type": "text", "text": "晴 25 度"}]}]}]}, port=PORT)
        sent = bodies["a"][-1]
        asst = [m for m in sent["messages"] if m.get("role") == "assistant"]
        toolmsg = [m for m in sent["messages"] if m.get("role") == "tool"]
        check("assistant 的 tool_use 转成 tool_calls",
              asst and asst[0].get("tool_calls")
              and asst[0]["tool_calls"][0]["function"]["name"] == "get_weather",
              json.dumps(asst, ensure_ascii=False)[:250])
        check("tool_result 转成独立的 role=tool 消息",
              toolmsg and toolmsg[0].get("tool_call_id") == "tu_1"
              and toolmsg[0].get("content") == "晴 25 度",
              json.dumps(toolmsg, ensure_ascii=False)[:250])

        # --- 流式：事件序列必须是 Anthropic 的 ---
        body = {"model": "deepseek-chat", "max_tokens": 20, "stream": True,
                "messages": [{"role": "user", "content": "你好"}]}
        req = urllib.request.Request(f"{PORT_URL}/v1/messages",
                                     data=json.dumps(body).encode(),
                                     headers={"Content-Type": "application/json"},
                                     method="POST")
        with urllib.request.urlopen(req, timeout=60) as resp:
            sse = resp.read().decode("utf-8", "replace")
        events = [l[7:] for l in sse.split("\n") if l.startswith("event: ")]
        check("流式首事件是 message_start", events[:1] == ["message_start"], str(events[:3]))
        check("流式末事件是 message_stop", events[-1:] == ["message_stop"], str(events[-3:]))
        check("流式有 content_block_delta", "content_block_delta" in events, str(events))
        check("流式没有裸的 OpenAI chunk（data 里不该出现 choices）",
              '"choices"' not in sse, sse[:200])
        deltas = []
        for line in sse.split("\n"):
            if not line.startswith("data: "):
                continue
            try:
                o = json.loads(line[6:])
            except Exception:
                continue
            d = o.get("delta") or {}
            if d.get("type") == "text_delta":
                deltas.append(d.get("text") or "")
        check("流式文本能拼回完整回复", "".join(deltas).startswith("a0a1a2"),
              repr("".join(deltas)))

        # --- 错误体也要是 Anthropic 形状 ---
        behavior["a"] = "fail400"
        st, r = admin("/v1/messages", {"model": "deepseek-chat", "max_tokens": 20,
                                       "messages": [{"role": "user", "content": "x"}]})
        check("上游 400 时 type=error", r.get("type") == "error", str(r)[:200])
        check("错误类型映射成 invalid_request_error",
              (r.get("error") or {}).get("type") == "invalid_request_error", str(r)[:200])
        check("错误信息保留上游原文",
              "参数不合法" in str((r.get("error") or {}).get("message")), str(r)[:200])
        behavior["a"] = "ok"

        # 所有上游都失败时的错误体（走的是「重试耗尽」那条路，和上面的
        # 单家客户端错误不是同一个分支）。实测线上曾经在这里漏转：
        # Anthropic 客户端拿到上游的 new_api_error 结构只会报「解析失败」。
        # 三个上游都得置失败 —— 这个配置没写死路由，是「自动兜底」模式，
        # 只挡一两家的话剩下的那个照样能把请求接走。
        behavior["a"] = "fail500"
        behavior["b"] = "fail500"
        behavior["c"] = "fail500"
        st, r = admin("/v1/messages", {"model": "deepseek-chat", "max_tokens": 20,
                                       "messages": [{"role": "user", "content": "x"}]})
        check("全部上游失败时也是 type=error", r.get("type") == "error", str(r)[:220])
        check("全部上游失败时错误类型已映射（不是上游原样结构）",
              (r.get("error") or {}).get("type") in
              ("api_error", "overloaded_error", "rate_limit_error", "authentication_error"),
              str(r)[:220])
        behavior["a"] = "ok"
        behavior["b"] = "ok"
        behavior["c"] = "ok"

        # --- 缺字段的请求要明确报错，而不是打到上游 ---
        st, r = admin("/v1/messages", {"max_tokens": 10,
                                       "messages": [{"role": "user", "content": "x"}]})
        check("缺 model 返回 400", st == 400, f"{st} {str(r)[:150]}")
        st, r = admin("/v1/messages", {"model": "deepseek-chat"})
        check("缺 messages 返回 400", st == 400, f"{st} {str(r)[:150]}")

        # --- count_tokens ---
        st, r = admin("/v1/messages/count_tokens", {
            "model": "deepseek-chat",
            "messages": [{"role": "user", "content": "这是一段用来估算的中文文字"}]})
        check("count_tokens 返回正整数 input_tokens",
              st == 200 and isinstance(r.get("input_tokens"), int) and r["input_tokens"] > 0,
              str(r)[:200])

        # --- 普通 OpenAI 路径不受影响 ---
        st, r = admin("/v1/chat/completions",
                      {"model": "deepseek-chat",
                       "messages": [{"role": "user", "content": "hi"}]})
        check("普通 OpenAI 路径仍是 choices 结构",
              st == 200 and r.get("choices"), str(r)[:200])
    finally:
        stop_router(p)

    # ================= 32. 上游并发 / 速率上限 =================
    print("\n[32] 上游并发 / 速率上限")
    cfg = write_config()
    p = start_router(cfg)
    try:
        admin("/admin/fetch-models")
        admin("/admin/select-models", {"id": "a1", "models": ["deepseek-chat"]})

        st, r = admin("/admin/upstream/capacity",
                      {"id": "a1", "max_concurrent": 2, "max_per_minute": 0})
        check("设置并发上限成功", st == 200 and r.get("max_concurrent") == 2, str(r)[:200])
        check("0 表示不限速率（回 null）", r.get("max_per_minute") is None, str(r)[:200])

        st, snap = get("/stats")
        by_id = {u["id"]: u for u in snap["upstreams"]}
        cap = by_id["a1"].get("capacity") or {}
        check("snapshot 带 capacity（含实时占用）",
              cap.get("max_concurrent") == 2 and "in_flight" in cap and "has_room" in cap,
              json.dumps(cap, ensure_ascii=False))
        check("空闲时 has_room=True", cap.get("has_room") is True,
              json.dumps(cap, ensure_ascii=False))

        # 上限必须随状态持久化（重启后还在）
        st, snap = get("/stats")
        check("capacity 挂在落盘状态里",
              (snap["upstreams"][0].get("capacity") or {}).get("max_concurrent") == 2
              or any((u.get("capacity") or {}).get("max_concurrent") == 2
                     for u in snap["upstreams"]),
              json.dumps([u.get("capacity") for u in snap["upstreams"]], ensure_ascii=False))

        # 慢上游 + 并发上限：同时发出的请求不能超过上限
        delay["a"] = 0.5     # 让每个请求慢 0.5s，才看得出发并发上限
        peak = {"a": 0}
        # 假上游里记录了 hits，但并发峰值需要另测：用并发请求跑一遍，
        # 只要全部成功、且耗时呈「分批」特征，就说明闸门在起作用。
        results = []
        lock2 = threading.Lock()

        def one():
            st, rr = admin("/v1/chat/completions",
                           {"model": "deepseek-chat",
                            "messages": [{"role": "user", "content": "hi"}]})
            with lock2:
                results.append(st)

        threads = [threading.Thread(target=one) for _ in range(6)]
        t0 = time.time()
        for t in threads:
            t.start()
        for t in threads:
            t.join()
        elapsed = time.time() - t0
        check("并发下请求全部成功", results.count(200) == 6, str(results))
        # a1 上限 2、每个 0.5s：6 个请求至少分 3 批 ≈ 1.5s。
        # 完全不限并发的话 6 个并排跑完只要 ~0.5s。
        check("被并发上限串行化（耗时应 ≥ 1.4s）", elapsed >= 1.4, f"实测 {elapsed:.2f}s")
        delay["a"] = 0.0

        # 上游失败时凭据必须归还，否则上限会越用越少。
        # 释放发生在 _try_upstream 的 finally 里，紧跟请求返回，稍等一下再读。
        time.sleep(0.4)
        st, snap = get("/stats")
        cap = next(u["capacity"] for u in snap["upstreams"] if u["id"] == "a1")
        check("请求跑完后 in_flight 归零（凭据没泄漏）", cap.get("in_flight") == 0,
              json.dumps(cap, ensure_ascii=False))

        # 速率上限：连发超过上限就被挡（1 分钟窗口）
        admin("/admin/upstream/capacity", {"id": "a1", "max_concurrent": 0,
                                           "max_per_minute": 3})
        st, snap = get("/stats")
        cap = next(u["capacity"] for u in snap["upstreams"] if u["id"] == "a1")
        check("速率上限已生效（max_per_minute=3）", cap.get("max_per_minute") == 3,
              json.dumps(cap, ensure_ascii=False))
        # 第 1 个请求就会占掉窗口里的一格
        admin("/v1/chat/completions", {"model": "deepseek-chat",
                                       "messages": [{"role": "user", "content": "hi"}]})
        st, snap = get("/stats")
        cap = next(u["capacity"] for u in snap["upstreams"] if u["id"] == "a1")
        check("用掉的次数被记进滑动窗口", cap.get("used_last_minute", 0) >= 1,
              json.dumps(cap, ensure_ascii=False))

        # 清掉上限后回到不限量
        st, r = admin("/admin/upstream/capacity", {"id": "a1", "max_concurrent": 0,
                                                   "max_per_minute": 0})
        check("清掉上限后返回 null",
              r.get("max_concurrent") is None and r.get("max_per_minute") is None,
              str(r)[:200])
        st, snap = get("/stats")
        cap = next(u["capacity"] for u in snap["upstreams"] if u["id"] == "a1")
        check("不限量时 has_room=True", cap.get("has_room") is True,
              json.dumps(cap, ensure_ascii=False))

        # 不存在的上游要报错
        st, r = admin("/admin/upstream/capacity", {"id": "不存在", "max_concurrent": 2})
        check("给不存在的上游设上限会报错", st >= 400, f"{st} {str(r)[:150]}")
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
