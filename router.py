#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
llm-router —— 多上游 LLM API 路由代理（零依赖，只用 Python 标准库）

核心思路：
    上游（Upstream）只管填「名字 / URL / API Key」，不关心模型。
    拉取一次模型列表后，按「模型」来配置路由：
        客户端请求 deepseek-chat → 找到配了 deepseek-chat 的上游 → 按你排的顺序调用。

用法：
    python router.py                      # 用默认 config.toml
    python router.py -c my.toml
    python router.py -c config.toml --check   # 只校验并打印，不启动

打开 http://127.0.0.1:8010/ 就是网页控制台。
"""

from __future__ import annotations

import argparse
import collections
import concurrent.futures
import http.client
import json
import os
import random
import re
import shutil
import socket
import sys
import threading
import time
import tomllib
import urllib.parse
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any, Optional

__version__ = "2.0.0"

WEBUI_FILENAME = "webui.html"

# 上游返回这些状态码时，认为「这个上游有问题」，换下一个
DEFAULT_RETRY_STATUS = {401, 403, 408, 409, 425, 429, 500, 502, 503, 504, 520, 521, 522, 523, 524}

VALID_STRATEGIES = ("priority", "round_robin", "random")


class Retryable(Exception):
    """表示这个上游没成功、应该换下一个。"""

    def __init__(self, upstream: "Upstream", status: int, body: bytes, reason: str = ""):
        super().__init__(reason or str(status))
        self.upstream = upstream
        self.status = status
        self.body = body
        self.reason = reason


def mask_key(key: str) -> str:
    """Key 只露头尾，避免页面上把密钥刷出去。"""
    if not key:
        return ""
    if len(key) <= 10:
        return key[:2] + "*" * max(0, len(key) - 2)
    return f"{key[:6]}…{key[-4:]}"


# 历史记录里输入/输出的截断长度：够看清内容，又不至于把内存吃满
HISTORY_TEXT_LIMIT = 4000


def extract_usage(obj: Any) -> dict:
    """从上游返回里挖出 token 用量。

    不同厂商字段名不统一，这里都认：
        prompt_tokens / completion_tokens / total_tokens      （OpenAI 标准）
        prompt_cache_hit_tokens / prompt_cache_miss_tokens    （DeepSeek）
        cache_read_input_tokens / cache_creation_input_tokens （Anthropic 风格）
        usage.prompt_tokens_details.cached_tokens             （嵌套写法）
    """
    if not isinstance(obj, dict):
        return {}
    u = obj.get("usage")
    if not isinstance(u, dict):
        return {}

    def num(*keys) -> Optional[int]:
        for k in keys:
            v = u.get(k)
            if isinstance(v, (int, float)) and not isinstance(v, bool):
                return int(v)
        return None

    out: dict[str, Optional[int]] = {}
    inp = num("prompt_tokens", "input_tokens")
    outp = num("completion_tokens", "output_tokens")
    total = num("total_tokens")
    if inp is not None:
        out["input"] = inp
    if outp is not None:
        out["output"] = outp
    if total is None and (inp is not None or outp is not None):
        total = (inp or 0) + (outp or 0)
    if total is not None:
        out["total"] = total

    # 缓存读取：各家叫法不同
    cache_read = num("cache_read_input_tokens", "prompt_cache_hit_tokens")
    if cache_read is None:
        details = u.get("prompt_tokens_details")
        if isinstance(details, dict):
            v = details.get("cached_tokens")
            if isinstance(v, (int, float)) and not isinstance(v, bool):
                cache_read = int(v)
    if cache_read is not None:
        out["cache_read"] = cache_read

    # 缓存写入：主要是 Anthropic 风格
    cache_write = num("cache_creation_input_tokens",
                      "cache_write_tokens", "prompt_cache_miss_tokens")
    if cache_write is not None:
        out["cache_write"] = cache_write

    return {k: v for k, v in out.items() if v is not None}


def messages_to_text(payload: Any) -> str:
    """把请求里的 messages 拼成一段可读文字。"""
    if not isinstance(payload, dict):
        return ""
    msgs = payload.get("messages")
    if isinstance(msgs, list):
        parts: list[str] = []
        for m in msgs:
            if not isinstance(m, dict):
                continue
            role = str(m.get("role") or "?")
            content = m.get("content")
            text = ""
            if isinstance(content, str):
                text = content
            elif isinstance(content, list):
                # 多模态：把文字部分取出来，图片之类的只记个标记
                chunks: list[str] = []
                for c in content:
                    if isinstance(c, dict):
                        if c.get("type") == "text":
                            chunks.append(str(c.get("text") or ""))
                        elif c.get("type") in ("image_url", "image"):
                            chunks.append("[图片]")
                    elif isinstance(c, str):
                        chunks.append(c)
                text = " ".join(chunks)
            parts.append(f"{role}: {text}")
        if parts:
            return "\n".join(parts)

    # 有些接口用 prompt / input 而不是 messages
    for key in ("prompt", "input", "query"):
        v = payload.get(key)
        if isinstance(v, str) and v:
            return v
    return ""


def response_to_text(body: bytes) -> tuple[str, dict]:
    """从非流式响应里取出回复文本和 token 用量。"""
    try:
        obj = json.loads(body)
    except Exception:
        return "", {}
    if not isinstance(obj, dict):
        return "", {}

    usage = extract_usage(obj)
    text = ""
    reasoning = ""
    choices = obj.get("choices")
    if isinstance(choices, list) and choices:
        first = choices[0]
        if isinstance(first, dict):
            msg = first.get("message")
            if isinstance(msg, dict):
                c = msg.get("content")
                if isinstance(c, str):
                    text = c
                elif isinstance(c, list):
                    text = " ".join(
                        str(x.get("text") or "") for x in c if isinstance(x, dict))
                # 推理模型的思考过程
                rc = msg.get("reasoning_content") or msg.get("reasoning")
                if isinstance(rc, str):
                    reasoning = rc
            if not text:
                t = first.get("text")
                if isinstance(t, str):
                    text = t
    # Anthropic 风格
    if not text:
        content = obj.get("content")
        if isinstance(content, list):
            text = " ".join(str(x.get("text") or "") for x in content
                            if isinstance(x, dict) and x.get("type") == "text")
    return text, usage, reasoning


def make_sse_collector() -> dict:
    """收集流式响应里的文本和 usage（后面的 chunk 通常带 usage）。"""
    return {"text": [], "reasoning": [], "usage": {}, "len": 0}


def collect_sse(chunk: bytes, acc: dict) -> None:
    """从 SSE 原始分片里累积文本和 usage 到 acc。"""
    acc["len"] += len(chunk)
    text = chunk.decode("utf-8", "replace")
    for line in text.split("\n"):
        line = line.strip()
        if not line.startswith("data:"):
            continue
        payload = line[5:].strip()
        if not payload or payload == "[DONE]":
            continue
        try:
            obj = json.loads(payload)
        except Exception:
            continue
        if not isinstance(obj, dict):
            continue
        # usage 可能出现多次，取最后一次非空的
        got = extract_usage(obj)
        if got:
            acc["usage"] = got
        choices = obj.get("choices")
        if isinstance(choices, list) and choices:
            first = choices[0]
            if isinstance(first, dict):
                delta = first.get("delta")
                if not isinstance(delta, dict):
                    # 有些厂商非流式也会走这里，message 兜一下
                    delta = first.get("message")
                if isinstance(delta, dict):
                    c = delta.get("content")
                    if isinstance(c, str) and c:
                        acc["text"].append(c)
                    # 推理模型（DeepSeek-R1 这类）先吐一段思考过程，
                    # 它不是最终回答，但记下来有助于看明白这次调用
                    rc = delta.get("reasoning_content") or delta.get("reasoning")
                    if isinstance(rc, str) and rc:
                        acc["reasoning"].append(rc)


def header_value(value: Any) -> str:
    """HTTP 响应头只能是 ASCII。

    BaseHTTPRequestHandler 用 latin-1 严格编码响应头，一旦值里有中文
    （比如上游名叫「DeepSeek 官方」）就会抛 UnicodeEncodeError，响应发到一半就断。
    这里把非 ASCII 部分百分号编码，中文名字照样能传出去。
    """
    text = str(value)
    try:
        text.encode("ascii")
        return text
    except UnicodeEncodeError:
        return urllib.parse.quote(text, safe="")


def norm_base_url(url: str) -> str:
    return str(url or "").strip().rstrip("/")


def api_paths(base_url: str, path: str) -> list[tuple[str, str]]:
    """把 base_url 和请求路径拼成候选上游地址，按可能性从高到低排。

    返回 [(完整目标, 相对 base 的后缀), ...]，后缀用来记住哪种拼法可用。

    麻烦之处在于 /v1 可能出现在 base 里、也可能出现在请求路径里，还可能两处都有：
        base = https://api.deepseek.com      path = /v1/chat/completions  → 直接拼
        base = https://api.siliconflow.cn/v1 path = /v1/chat/completions  → 去掉路径里那份
        base = https://…/api/v3              path = /v1/chat/completions  → 去掉路径里那份
        base = https://api.deepseek.com      path = /models   （内部调用）→ 补上 /v1
    """
    base = base_url.rstrip("/")
    if not path.startswith("/"):
        path = "/" + path

    base_has_ver = bool(re.search(r"/v\d+(?:beta|alpha)?$", base))
    path_ver = ""
    for ver in ("/v1beta", "/v1"):
        if path == ver or path.startswith(ver + "/"):
            path_ver = ver
            break
    stripped = (path[len(path_ver):] or "/") if path_ver else path

    out: list[tuple[str, str]] = []

    def add(suffix: str) -> None:
        if suffix and not any(s == suffix for _, s in out):
            out.append((base + suffix, suffix))

    # base 里有 /v3、路径里写的是 /v1 这种情况（火山方舟就是 /api/v3），
    # 两边版本号不一致时也要能拼上，所以除了去掉路径里的版本段，
    # 还要准备「把路径的版本段换成 base 的版本段」这种候选。
    base_ver = ""
    m_base = re.search(r"(/v\d+(?:beta|alpha)?)$", base)
    if m_base:
        base_ver = m_base.group(1)

    if path_ver:
        if base_has_ver:
            add(stripped)                     # 推荐：去掉路径里重复的版本段
            add(path)                         # 万一是子路径，比如 /v1/foo 这种真路径
            if base_ver and base_ver != path_ver:
                add(base_ver + stripped)      # 用 base 的版本号重拼
        else:
            add(path)                         # 版本号只在路径里
            add(stripped)
    else:
        if base_has_ver:
            add(path)
        else:
            add("/v1" + path)                 # 内部调用（如 /models）补上 /v1
            add(path)

    return out


# ---------------------------------------------------------------------------
# 上游
# ---------------------------------------------------------------------------

class Upstream:
    """一个上游来源：名字、地址、Key。模型由拉取得到。"""

    def __init__(self, cfg: dict, index: int = 0):
        self.id: str = str(cfg.get("id") or f"u{index + 1}")
        self.name: str = str(cfg.get("name") or self.id)
        self.base_url: str = norm_base_url(cfg.get("base_url") or "")
        self.api_key: str = str(cfg.get("api_key") or "")
        self.enabled: bool = bool(cfg.get("enabled", True))

        # 拉取到的模型列表（运行时状态，会持久化）
        raw_models = cfg.get("available_models") or []
        self.available_models: list[str] = [str(m) for m in raw_models]
        self.models_fetched_at: float = float(cfg.get("models_fetched_at") or 0.0)
        self.models_error: Optional[str] = cfg.get("models_error")

        # 用户勾选了哪些模型要用（存的是上游这边的真实模型名）。
        # None 表示「状态文件里没这个字段」（升级前的老状态），
        # 到 _apply_state 里再按已有的模型路由推断出来，不能简单地全选——
        # 那样会把上游的全部模型（可能上百个）一股脑塞进路由列表。
        raw_selected = cfg.get("selected_models")
        self.selected_models: Optional[list[str]] = (
            [str(m) for m in raw_selected] if raw_selected is not None else None
        )

        # 真实模型名 → 分组名。
        # 同一个「deepseek」在各家可能叫 ds1/ds2/ds3，靠这张表归到一个分组；
        # 没写映射的就用真实名本身当分组名（一对一）。
        raw_groups = cfg.get("model_groups") or {}
        self.model_groups: dict[str, str] = {
            str(k): str(v) for k, v in raw_groups.items() if str(k) and str(v)
        }

        # 记录哪种路径拼法可用（运行时，不持久化）
        self._prefix_hint: Optional[str] = None

        # 统计
        self._lock = threading.Lock()
        self.requests = 0
        self.successes = 0
        self.failures = 0
        self.consecutive_failures = 0
        self.cooldown_until = 0.0
        self.last_error: Optional[str] = None
        self.last_used: float = 0.0
        self.total_latency = 0.0
        self.latency_samples = 0

        # 每上游可单独覆盖的超时/冷却
        self.connect_timeout: float = float(cfg.get("connect_timeout_seconds") or 10)
        self.read_timeout: float = float(cfg.get("read_timeout_seconds") or 300)
        self.cooldown: float = float(cfg.get("cooldown_seconds") or 60)

    # ---- 可用性 ----
    def available(self) -> bool:
        return self.enabled and time.time() >= self.cooldown_until

    def cooling_remaining(self) -> float:
        return max(0.0, self.cooldown_until - time.time())

    def group_of(self, real_model: str) -> str:
        """上游真实模型名 → 分组名。没设映射就是它自己。"""
        return self.model_groups.get(real_model, real_model)

    def set_group(self, real_model: str, group: Optional[str]) -> None:
        """把某个真实模型归到指定分组；group 为空/相同则取消映射（回到原名）。"""
        if not real_model:
            return
        g = (group or "").strip()
        if not g or g == real_model:
            self.model_groups.pop(real_model, None)
        else:
            self.model_groups[real_model] = g

    def groups(self) -> dict[str, list[str]]:
        """分组名 → 这个上游在这个分组下的真实模型名列表。"""
        out: dict[str, list[str]] = {}
        for m in self.selected():
            out.setdefault(self.group_of(m), []).append(m)
        return out

    def has_group(self, group: str) -> bool:
        return any(self.group_of(m) == group for m in self.selected())

    def real_models_in(self, group: str) -> list[str]:
        return [m for m in self.selected() if self.group_of(m) == group]

    def selected(self) -> list[str]:
        """当前勾选的模型。None（老状态）时按可用模型兜底，保证不炸。"""
        if self.selected_models is None:
            return list(self.available_models)
        return self.selected_models

    def has_model(self, model: str) -> bool:
        """这个上游能不能接这个模型：拉取到了，而且用户勾选了要用。"""
        return model in self.selected()

    def is_offered(self, model: str) -> bool:
        """上游有这个模型（不管用户勾没勾）。"""
        return model in self.available_models

    def set_selected(self, models: list[str]) -> list[str]:
        """设置勾选的模型。只会保留上游确实有的。"""
        allowed = [str(m) for m in models if str(m) in self.available_models]
        self.selected_models = allowed
        return allowed

    def select_all(self) -> list[str]:
        self.selected_models = list(self.available_models)
        return self.selected_models

    def select_none(self) -> list[str]:
        self.selected_models = []
        return self.selected_models

    # ---- 统计 ----
    def _record(self, ok: Optional[bool], latency: float = 0.0, error: Optional[str] = None,
                threshold: int = 3, cooldown: float = 0.0) -> None:
        with self._lock:
            self.requests += 1
            self.last_used = time.time()
            if ok is True:
                self.successes += 1
                self.consecutive_failures = 0
                self.cooldown_until = 0.0
                if latency > 0:
                    self.total_latency += latency
                    self.latency_samples += 1
            elif ok is False:
                self.failures += 1
                self.consecutive_failures += 1
                self.last_error = error
                if cooldown > 0 and self.consecutive_failures >= max(1, threshold):
                    self.cooldown_until = time.time() + cooldown
            else:
                self.last_error = error

    def record_success(self, latency: float = 0.0) -> None:
        self._record(True, latency=latency)

    def record_failure(self, error: str, threshold: int = 3, cooldown: float = 0.0) -> None:
        self._record(False, error=error, threshold=threshold, cooldown=cooldown)

    def record_neutral(self, note: str) -> None:
        self._record(None, error=note)

    def reset_stats(self) -> None:
        with self._lock:
            self.requests = self.successes = self.failures = 0
            self.consecutive_failures = 0
            self.cooldown_until = 0.0
            self.last_error = None
            self.total_latency = 0.0
            self.latency_samples = 0

    def snapshot(self) -> dict:
        with self._lock:
            avg = (self.total_latency / self.latency_samples) if self.latency_samples else 0.0
            return {
                "id": self.id,
                "name": self.name,
                "base_url": self.base_url,
                "enabled": self.enabled,
                "key_hint": mask_key(self.api_key),
                "has_key": bool(self.api_key),
                "available_models": list(self.available_models),
                "selected_models": list(self.selected()),
                "selected_count": len(self.selected()),
                "available_count": len(self.available_models),
                # 真实模型名 → 分组名，以及分组汇总，前端要用
                "model_groups": dict(self.model_groups),
                "groups": {g: sorted(ms) for g, ms in sorted(self.groups().items())},
                "models_fetched_at": self.models_fetched_at or None,
                "models_error": self.models_error,
                "requests": self.requests,
                "successes": self.successes,
                "failures": self.failures,
                "consecutive_failures": self.consecutive_failures,
                "cooldown_remaining_seconds": round(self.cooling_remaining(), 1),
                "available": self.available(),
                "last_error": self.last_error,
                "avg_latency_seconds": round(avg, 3),
                "last_used": self.last_used or None,
            }

    def state(self) -> dict:
        """要持久化的部分。"""
        return {
            "id": self.id,
            "name": self.name,
            "base_url": self.base_url,
            "api_key": self.api_key,
            "enabled": self.enabled,
            "available_models": list(self.available_models),
            "selected_models": list(self.selected()),
            "model_groups": dict(self.model_groups),
            "models_fetched_at": self.models_fetched_at,
            "models_error": self.models_error,
        }

    # ---- 请求上游 ----
    def headers(self, content_type: str = "application/json",
                accept: str = "application/json") -> dict[str, str]:
        h = {
            "Content-Type": content_type,
            "Accept": accept,
            # 关键：不要 gzip，否则 SSE 会被缓冲，流式就废了
            "Accept-Encoding": "identity",
            "User-Agent": f"llm-router/{__version__}",
        }
        if self.api_key:
            h["Authorization"] = f"Bearer {self.api_key}"
        return h

    def open(self, method: str, path: str, data: Optional[bytes] = None,
             headers: Optional[dict] = None):
        """连上游发一个请求。返回 (conn, resp, target_path)。"""
        parts = urllib.parse.urlsplit(self.base_url)
        if parts.scheme not in ("http", "https") or not parts.hostname:
            raise ValueError(f"上游地址不合法: {self.base_url!r}")
        scheme = parts.scheme.lower()
        port = parts.port or (443 if scheme == "https" else 80)
        cls = http.client.HTTPSConnection if scheme == "https" else http.client.HTTPConnection

        candidates = api_paths(self.base_url, path)
        if self._prefix_hint:
            # 上次成功的后缀优先试，省一次 404 往返
            for i, (_, suffix) in enumerate(candidates):
                if suffix == self._prefix_hint:
                    candidates.insert(0, candidates.pop(i))
                    break

        last_exc: Optional[Exception] = None
        last_resp = None
        for target, suffix in candidates:
            conn = None
            try:
                conn = cls(parts.hostname, port, timeout=self.connect_timeout)
                conn.connect()
                try:
                    conn.sock.settimeout(self.read_timeout)
                except Exception:
                    pass
                conn.request(method, target, body=data, headers=headers or {})
                resp = conn.getresponse()
                if resp.status == 404 and len(candidates) > 1:
                    # 路径拼法不对，记下来换下一种；把响应留着，
                    # 万一全都不行，至少能把这个 404 原样透传出去
                    last_resp = (conn, resp, target, suffix)
                    continue
                self._prefix_hint = suffix
                return conn, resp, target
            except Exception as exc:  # noqa: BLE001
                last_exc = exc
                self._close(conn)
                continue

        if last_resp is not None:
            conn, resp, target, suffix = last_resp
            self._prefix_hint = suffix
            return conn, resp, target
        if last_exc is not None:
            raise last_exc
        raise RuntimeError("没有可用的上游路径")

    @staticmethod
    def _close(conn) -> None:
        try:
            if conn is not None:
                conn.close()
        except Exception:
            pass

    # ---- 拉取模型列表 ----
    def fetch_models(self) -> dict:
        """GET {base}/v1/models，解析成模型 id 列表。"""
        conn = None
        try:
            conn, resp, _ = self.open("GET", "/models", headers=self.headers())
            body = resp.read()
            if resp.status >= 400:
                detail = ""
                try:
                    err = json.loads(body)
                    e = err.get("error")
                    detail = (e.get("message") if isinstance(e, dict) else e) or err.get("message") or ""
                except Exception:
                    detail = body[:200].decode("utf-8", "replace")
                raise RuntimeError(f"HTTP {resp.status}{': ' + str(detail) if detail else ''}")

            try:
                parsed = json.loads(body)
            except Exception as exc:  # noqa: BLE001
                raise RuntimeError(f"返回的不是合法 JSON: {exc}") from exc

            ids = _extract_ids(parsed)
            if not ids:
                raise RuntimeError("没有解析到任何模型 id")

            ids = sorted(set(ids))
            self.available_models = ids
            self.models_fetched_at = time.time()
            self.models_error = None
            # 上游下架了的模型，勾选里也去掉，免得留着一个永远调不通的配置
            prev = self.selected()
            keep = set(ids)
            still_there = [m for m in prev if m in keep]
            dropped = [m for m in prev if m not in keep]
            self.selected_models = still_there
            return {"ok": True, "models": ids, "count": len(ids),
                    "selected": still_there,
                    "dropped_selections": dropped}
        except Exception as exc:  # noqa: BLE001
            msg = f"{type(exc).__name__}: {exc}"
            self.models_error = msg
            return {"ok": False, "error": msg, "models": list(self.available_models)}
        finally:
            self._close(conn)

    # ---- 测速 ----
    def ping(self, model: Optional[str] = None) -> dict:
        target = model or (self.available_models[0] if self.available_models else None)
        if not target:
            return {"id": self.id, "name": self.name, "ok": False,
                    "latency_seconds": None, "error": "还不知道该用哪个模型，先拉取模型列表"}
        payload = json.dumps(
            {"model": target, "messages": [{"role": "user", "content": "hi"}], "max_tokens": 1},
            ensure_ascii=False,
        ).encode("utf-8")
        conn = None
        t0 = time.time()
        try:
            conn, resp, _ = self.open("POST", "/chat/completions", data=payload,
                                      headers=self.headers())
            latency = time.time() - t0
            body = resp.read()
            ok = resp.status < 400
            error = None
            if not ok:
                try:
                    parsed = json.loads(body)
                    err = parsed.get("error")
                    if isinstance(err, dict):
                        error = err.get("message") or json.dumps(err, ensure_ascii=False)
                    elif err:
                        error = str(err)
                except Exception:
                    pass
                if error is None:
                    error = f"HTTP {resp.status}"
            return {"id": self.id, "name": self.name, "ok": ok, "status": resp.status,
                    "latency_seconds": round(latency, 3), "model": target, "error": error}
        except Exception as exc:  # noqa: BLE001
            return {"id": self.id, "name": self.name, "ok": False, "latency_seconds": None,
                    "model": target, "error": f"{type(exc).__name__}: {exc}"}
        finally:
            self._close(conn)


def _extract_ids(parsed: Any) -> list[str]:
    """从各种形状的响应里挖出模型 id。

    见过的样子：
        {"data": [{"id": "gpt-4"}]}
        {"models": [{"name": "glm-4"}]}
        {"result": {"models": [{"name": "..."}]}}     # 嵌套一层
        ["model-a", "model-b"]
    """
    if isinstance(parsed, str):
        return [parsed] if parsed else []

    if isinstance(parsed, list):
        ids: list[str] = []
        for it in parsed:
            ids.extend(_extract_ids(it))
        return ids

    if not isinstance(parsed, dict):
        return []

    # 常见的「列表字段」名字，先直接找数组
    for key in ("data", "models", "result", "items", "list", "model_list"):
        if key not in parsed:
            continue
        value = parsed[key]
        # result/models 可能是包了一层的对象
        if isinstance(value, dict):
            nested = _extract_ids(value)
            if nested:
                return nested
            continue
        got = _extract_ids(value)
        if got:
            return got

    # 本身就是一条记录
    for key in ("id", "name", "model", "model_name"):
        v = parsed.get(key)
        if isinstance(v, str) and v:
            return [v]
    return []


# ---------------------------------------------------------------------------
# 模型路由
# ---------------------------------------------------------------------------

class RouteEntry:
    """一个分组里的一个落点：某个上游 + 那边这个模型实际叫什么。

    分组名（客户端调用的名字）和这里的 model（上游真实名字）是两回事：
        deepseek 这个分组 →  A 上游叫 ds1，B 上游叫 ds2，C 上游叫 ds3
    数组顺序 = 调用顺序。
    """

    def __init__(self, cfg: dict):
        self.upstream_id: str = str(cfg.get("upstream") or "")
        # 上游那边的真实模型名。分组名可能和它完全不同
        self.model: str = str(cfg.get("model") or "")
        self.enabled: bool = bool(cfg.get("enabled", True))

    def state(self) -> dict:
        return {"upstream": self.upstream_id, "model": self.model, "enabled": self.enabled}


class ModelRoute:
    """一个分组：客户端可见的名字 → 若干上游（各用自己那边的真实名字）。"""

    def __init__(self, name: str, cfg: Optional[dict] = None):
        cfg = cfg or {}
        self.name = name
        strategy = str(cfg.get("strategy") or "priority").lower()
        self.strategy = strategy if strategy in VALID_STRATEGIES else "priority"
        self.entries: list[RouteEntry] = [RouteEntry(e) for e in (cfg.get("entries") or [])]

    def entry_for(self, upstream_id: str, model: str) -> Optional[RouteEntry]:
        for e in self.entries:
            if e.upstream_id == upstream_id and e.model == model:
                return e
        return None

    def remove_entry(self, upstream_id: str, model: str) -> bool:
        before = len(self.entries)
        self.entries = [e for e in self.entries
                        if not (e.upstream_id == upstream_id and e.model == model)]
        return len(self.entries) != before

    def state(self) -> dict:
        return {
            "strategy": self.strategy,
            "entries": [e.state() for e in self.entries],
        }


# ---------------------------------------------------------------------------
# 路由器
# ---------------------------------------------------------------------------

class Router:
    def __init__(self, cfg: dict, config_path: str):
        self.config_path = config_path
        self.lock = threading.Lock()
        self.rr_counters: dict[str, int] = {}
        self.history: collections.deque = collections.deque(maxlen=200)
        self.started_at = time.time()
        self.total_requests = 0

        server_cfg = cfg.get("server") or {}
        self.host = str(server_cfg.get("host", "127.0.0.1"))
        self.port = int(server_cfg.get("port", 8010))
        self.log_requests = bool(server_cfg.get("log_requests", True))
        self.enable_cors = bool(server_cfg.get("enable_cors", False))
        self.max_body_mb = float(server_cfg.get("max_body_mb", 32))

        router_cfg = cfg.get("router") or {}
        self.failure_threshold = max(1, int(router_cfg.get("failure_threshold", 3)))
        self.cooldown_default = float(router_cfg.get("cooldown_seconds", 60))
        self.default_strategy = str(router_cfg.get("default_strategy", "priority")).lower()
        if self.default_strategy not in VALID_STRATEGIES:
            self.default_strategy = "priority"

        self.upstreams: list[Upstream] = []
        self.models: dict[str, ModelRoute] = {}

        seed = self._seed_from_config(cfg)
        saved = self._load_state()
        if saved:
            self._apply_state(saved)
        else:
            self._apply_state(seed)
            self._save_state()

    # ---- 配置载入 ----
    @staticmethod
    def _seed_from_config(cfg: dict) -> dict:
        ups = []
        for i, u in enumerate(cfg.get("upstreams") or cfg.get("sources") or []):
            models = [str(m) for m in (u.get("models") or [])]
            ups.append({
                "id": u.get("id") or f"u{i + 1}",
                "name": u.get("name") or f"上游{i + 1}",
                "base_url": u.get("base_url") or "",
                "api_key": u.get("api_key") or "",
                "enabled": u.get("enabled", True),
                "available_models": models,
                # 配置文件里写明的模型，视为「已经选用」，不然写完配置还得去网页勾一遍
                "selected_models": [str(m) for m in (u.get("selected_models") or models)],
            })
        models_out: dict[str, dict] = {}
        for name, mcfg in (cfg.get("models") or {}).items():
            models_out[name] = {
                "strategy": mcfg.get("strategy", "priority"),
                "entries": [
                    {"upstream": e.get("upstream"), "model": e.get("model") or name,
                     "enabled": e.get("enabled", True)}
                    for e in (mcfg.get("entries") or [])
                ],
            }
        return {"upstreams": ups, "models": models_out}

    def _apply_state(self, data: dict) -> None:
        self.upstreams = []
        for i, u in enumerate(data.get("upstreams") or []):
            up = Upstream(u, i)
            up.cooldown = self.cooldown_default
            if not up.base_url:
                sys.stderr.write(f"[warn] 上游 {up.name!r} 没有填 URL，已跳过\n")
                continue
            self.upstreams.append(up)
        self.models = {}
        for name, mcfg in (data.get("models") or {}).items():
            route = ModelRoute(str(name), mcfg)
            route.entries = [e for e in route.entries if e.upstream_id]
            self.models[str(name)] = route

        # 老状态文件没有 selected_models：按「已经在模型路由里用到的」推断勾选。
        # 不能简单地全选——上游动辄上百个模型，全勾上会把路由列表塞满；
        # 也不能全不选——那样原来配好的路由会被判定成「没勾选」而失效。
        adopted: list[str] = []
        for up in self.upstreams:
            if up.selected_models is not None:
                continue
            inferred: list[str] = []
            for name, route in self.models.items():
                for e in route.entries:
                    if e.upstream_id == up.id and name in up.available_models:
                        inferred.append(name)
                        break
            up.selected_models = sorted(set(inferred))
            if inferred:
                adopted.append(f"{up.name}({len(up.selected_models)})")
        if adopted:
            sys.stderr.write(
                "[info] 已把原有的模型路由同步成勾选状态: " + ", ".join(adopted) + "\n")

    # ---- 状态持久化 ----
    #
    # 状态文件（上游、模型路由、勾选、分组）统一放进配置同目录下的 .llmrouter/
    # 子目录，不跟普通文件混在一起。
    # 这样任何「按扩展名清理项目根目录」的操作（比如 rm *.state.json）都碰不到它。
    # 另外每次保存前会把上一版留成 .bak，reset 时也不是删除而是移进 trash/。

    STATE_DIRNAME = ".llmrouter"

    def _state_dir(self) -> str:
        directory = os.path.dirname(os.path.abspath(self.config_path))
        return os.path.join(directory, self.STATE_DIRNAME)

    def _state_stem(self) -> str:
        return os.path.splitext(os.path.basename(self.config_path))[0]

    def _state_path(self) -> str:
        return os.path.join(self._state_dir(), f"{self._state_stem()}.state.json")

    def _legacy_state_path(self) -> str:
        """旧版本把状态放在配置旁边，认得它以便自动搬过来。"""
        directory = os.path.dirname(os.path.abspath(self.config_path))
        return os.path.join(directory, f"{self._state_stem()}.state.json")

    def _trash_path(self, suffix: str) -> str:
        trash = os.path.join(self._state_dir(), "trash")
        os.makedirs(trash, exist_ok=True)
        stamp = time.strftime("%Y%m%d-%H%M%S")
        return os.path.join(trash, f"{self._state_stem()}.{suffix}.{stamp}.json")

    def _load_state(self) -> Optional[dict]:
        path = self._state_path()
        legacy = self._legacy_state_path()

        # 新位置没有、旧位置有 → 自动搬过来（一次性迁移）
        if not os.path.exists(path) and os.path.exists(legacy):
            try:
                os.makedirs(self._state_dir(), exist_ok=True)
                shutil.copyfile(legacy, path)
                sys.stderr.write(
                    f"[info] 已把状态文件迁移到 {self.STATE_DIRNAME}/ 子目录"
                    f"（原文件保留在 {os.path.basename(legacy)}，可自行删除）\n")
            except Exception as exc:  # noqa: BLE001
                sys.stderr.write(f"[warn] 状态文件迁移失败，继续用旧位置: {exc}\n")
                path = legacy

        if not os.path.exists(path):
            return None
        try:
            with open(path, "r", encoding="utf-8") as fh:
                data = json.load(fh)
            if not isinstance(data, dict):
                return None
            return data
        except Exception as exc:  # noqa: BLE001
            sys.stderr.write(f"[warn] {os.path.basename(path)} 读不了，改用配置文件: {exc}\n")
            return None

    def _save_state(self) -> None:
        payload = {
            "version": 2,
            "upstreams": [u.state() for u in self.upstreams],
            "models": {name: r.state() for name, r in self.models.items()},
        }
        path = self._state_path()
        try:
            os.makedirs(self._state_dir(), exist_ok=True)

            # 覆盖前先把上一版留成 .bak，这样万一写坏/误删还能回退一步
            if os.path.exists(path):
                try:
                    shutil.copyfile(path, path + ".bak")
                except Exception:  # noqa: BLE001
                    pass

            # 先写临时文件再原子替换，避免写一半断电留下半个文件
            tmp = path + ".tmp"
            with open(tmp, "w", encoding="utf-8") as fh:
                json.dump(payload, fh, ensure_ascii=False, indent=2)
            os.replace(tmp, path)
        except Exception as exc:  # noqa: BLE001
            sys.stderr.write(f"[warn] 保存状态失败（不影响本次运行）: {exc}\n")

    def reset_to_config(self) -> dict:
        """按 config.toml 重置：状态文件移进 trash/ 留底，而不是删掉。"""
        path = self._state_path()
        try:
            if os.path.exists(path):
                # 回收而非删除 —— 万一重置错了还能捞回来
                try:
                    shutil.move(path, self._trash_path("reset"))
                except Exception:  # noqa: BLE001
                    os.remove(path)
            with open(self.config_path, "rb") as fh:
                cfg = tomllib.load(fh)
            self._apply_state(self._seed_from_config(cfg))
            self._save_state()
            return {"ok": True, "upstreams": len(self.upstreams), "models": len(self.models)}
        except Exception as exc:  # noqa: BLE001
            return {"ok": False, "error": str(exc)}

    # ---- 查上游 ----
    def find_upstream(self, uid: Optional[str]) -> Optional[Upstream]:
        for u in self.upstreams:
            if u.id == uid:
                return u
        return None

    # ---- 上游增删改 ----
    def add_upstream(self, payload: dict) -> Upstream:
        name = str(payload.get("name") or "").strip() or f"上游{len(self.upstreams) + 1}"
        base_url = norm_base_url(payload.get("base_url") or "")
        if not base_url:
            raise ValueError("请填上游地址（base_url）")
        parts = urllib.parse.urlsplit(base_url)
        if parts.scheme not in ("http", "https") or not parts.hostname:
            raise ValueError(f"上游地址不合法: {base_url!r}")
        uid = str(payload.get("id") or "").strip() or f"u{uuid.uuid4().hex[:6]}"
        if self.find_upstream(uid):
            raise ValueError(f"上游 id {uid!r} 已存在")
        up = Upstream({
            "id": uid, "name": name, "base_url": base_url,
            "api_key": payload.get("api_key") or "",
            "enabled": payload.get("enabled", True),
            # 新加的上游默认一个模型都不选，等用户拉取后自己勾
            "available_models": [],
            "selected_models": [],
        }, len(self.upstreams))
        up.cooldown = self.cooldown_default
        self.upstreams.append(up)
        self._save_state()
        return up

    def update_upstream(self, payload: dict) -> Upstream:
        up = self.find_upstream(payload.get("id"))
        if up is None:
            raise KeyError(f"找不到上游 {payload.get('id')!r}")
        if payload.get("name") is not None:
            up.name = str(payload["name"]).strip() or up.name
        if payload.get("base_url") is not None:
            new_url = norm_base_url(payload["base_url"])
            if not new_url:
                raise ValueError("上游地址不能为空")
            parts = urllib.parse.urlsplit(new_url)
            if parts.scheme not in ("http", "https") or not parts.hostname:
                raise ValueError(f"上游地址不合法: {new_url!r}")
            if new_url != up.base_url:
                # 换了地址就是换了一家，之前拉到的模型和勾选都不作数了；
                # 顺便把它从各模型路由里摘掉，等重新拉取勾选后再参与。
                up._prefix_hint = None
                up.available_models = []
                up.selected_models = []
                up.models_fetched_at = 0.0
                up.models_error = None
                for name, route in list(self.models.items()):
                    route.entries = [e for e in route.entries if e.upstream_id != uid]
                    if not route.entries:
                        del self.models[name]
            up.base_url = new_url
        if payload.get("api_key") is not None:
            up.api_key = str(payload["api_key"])
        if payload.get("enabled") is not None:
            up.enabled = bool(payload["enabled"])
        self._save_state()
        return up

    def delete_upstream(self, uid: str) -> dict:
        up = self.find_upstream(uid)
        if up is None:
            raise KeyError(f"找不到上游 {uid!r}")
        self.upstreams = [u for u in self.upstreams if u.id != uid]
        # 把引用它的模型路由一并清掉
        removed: list[str] = []
        for name, route in list(self.models.items()):
            before = len(route.entries)
            route.entries = [e for e in route.entries if e.upstream_id != uid]
            if not route.entries:
                del self.models[name]
                removed.append(name)
            elif len(route.entries) != before:
                removed.append(f"{name}(减了{len(route.entries) - before}项)")
        self._save_state()
        return {"ok": True, "deleted": up.name, "affected_models": removed}

    # ---- 分组与路由同步 ----
    #
    # 心智模型很简单：
    #   上游侧：勾选哪些真实模型要用，并且可以给每个真实模型指定「归到哪个分组」
    #   分组侧：一个分组 = 一条模型路由，客户端就用分组名来调用
    #
    # 路由是从「勾选 + 分组」自动推导出来的，不需要手工维护成员；
    # 手工维护的只有每个分组内部的调用顺序。

    def sync_routes(self) -> dict:
        """按「勾选 + 分组」重建模型路由，保留已有的人工顺序。

        每个分组一条路由，成员 = 所有上游里「勾选了、且归到这个分组」的真实模型。
        已有顺序里的成员保持原来的相对位置，新成员按上游顺序追加在末尾。
        """
        target: dict[str, list[tuple[str, str]]] = {}
        for up in self.upstreams:
            for real in up.selected():
                target.setdefault(up.group_of(real), []).append((up.id, real))

        created: list[str] = []
        removed: list[str] = []

        # 没有成员的分组，路由直接删掉
        for group in list(self.models):
            if group not in target:
                del self.models[group]
                removed.append(group)

        for group, members in target.items():
            route = self.models.get(group)
            if route is None:
                self.models[group] = ModelRoute(group, {
                    "strategy": self.default_strategy,
                    "entries": [{"upstream": uid, "model": real, "enabled": True}
                                for uid, real in members],
                })
                created.append(group)
                continue

            wanted = set(members)
            kept = [e for e in route.entries if (e.upstream_id, e.model) in wanted]
            have = {(e.upstream_id, e.model) for e in kept}
            for uid, real in members:            # 新成员追加在末尾
                if (uid, real) not in have:
                    kept.append(RouteEntry({"upstream": uid, "model": real, "enabled": True}))
            route.entries = kept

        self._save_state()
        return {"created": created, "removed": removed}

    def apply_selected_models(self, uid: str, models: list[str]) -> dict:
        """设置某个上游勾选了哪些真实模型，然后同步路由。"""
        up = self.find_upstream(uid)
        if up is None:
            raise KeyError(f"找不到上游 {uid!r}")

        before = set(up.selected())
        after = set(up.set_selected(models))
        added, removed = sorted(after - before), sorted(before - after)

        sync = self.sync_routes()
        return {
            "ok": True,
            "id": uid,
            "selected": sorted(after),
            "added": added,
            "removed": removed,
            "created_models": sync["created"],
            "removed_models": sync["removed"],
        }

    def select_all_models(self, uid: str) -> dict:
        up = self.find_upstream(uid)
        if up is None:
            raise KeyError(f"找不到上游 {uid!r}")
        return self.apply_selected_models(uid, list(up.available_models))

    def set_model_group(self, uid: str, mapping: dict) -> dict:
        """设置「上游真实模型名 → 分组名」的映射。

        mapping 形如 {"ds1": "deepseek", "ds2": "deepseek", "gpt4": ""}，
        空字符串表示取消映射、回到用真实名当分组名。
        """
        up = self.find_upstream(uid)
        if up is None:
            raise KeyError(f"找不到上游 {uid!r}")
        if not isinstance(mapping, dict):
            raise ValueError("mapping 必须是对象")
        touched: list[str] = []
        for real, group in mapping.items():
            real = str(real).strip()
            if not real:
                continue
            # 和 save_model 一样：上游清单可能不全，用户填的名字要认
            if real not in up.available_models:
                up.available_models.append(real)
                up.available_models.sort()
            up.set_group(real, str(group) if group is not None else "")
            touched.append(real)
        sync = self.sync_routes()
        return {
            "ok": True, "id": uid, "touched": touched,
            "model_groups": dict(up.model_groups),
            "created_models": sync["created"], "removed_models": sync["removed"],
        }

    def rename_group(self, old: str, new: str) -> dict:
        """把某个分组整体改名（所有上游里归到这个分组的映射一起改）。"""
        old = str(old or "").strip()
        new = str(new or "").strip()
        if not old or not new:
            raise ValueError("分组名不能为空")
        if old == new:
            return {"ok": True, "renamed": old, "to": new, "entries": 0}

        n = 0
        for up in self.upstreams:
            # 显式映射过的
            for real, group in list(up.model_groups.items()):
                if group == old:
                    up.model_groups[real] = new
                    n += 1
            # 没映射、直接用真实名当分组名的
            for real in up.available_models:
                if real == old and real not in up.model_groups:
                    if new != real:
                        up.model_groups[real] = new
                        n += 1

        # 路由本身也改名，策略和顺序保留
        route = self.models.pop(old, None)
        if route is not None:
            route.name = new
            self.models[new] = route

        sync = self.sync_routes()
        return {"ok": True, "renamed": old, "to": new, "entries": n,
                "created_models": sync["created"], "removed_models": sync["removed"]}

    def select_no_models(self, uid: str) -> dict:
        up = self.find_upstream(uid)
        if up is None:
            raise KeyError(f"找不到上游 {uid!r}")
        return self.apply_selected_models(uid, [])

    # ---- 拉取模型 ----
    def fetch_models(self, ids: Optional[list[str]] = None, timeout: float = 60.0) -> list[dict]:
        targets = [u for u in self.upstreams if not ids or u.id in set(ids)]
        if not targets:
            return []
        results: list[dict] = []
        with concurrent.futures.ThreadPoolExecutor(max_workers=min(8, len(targets))) as pool:
            futures = {pool.submit(u.fetch_models): u for u in targets}
            for fut in concurrent.futures.as_completed(futures, timeout=timeout + 10):
                u = futures[fut]
                try:
                    r = fut.result()
                except Exception as exc:  # noqa: BLE001
                    r = {"ok": False, "error": f"{type(exc).__name__}: {exc}", "models": []}
                results.append({"id": u.id, "name": u.name, **r})
        order = {u.id: i for i, u in enumerate(self.upstreams)}
        results.sort(key=lambda r: order.get(r["id"], 999))
        self._save_state()
        return results

    # ---- 模型路由增删改 ----
    def save_model(self, payload: dict) -> ModelRoute:
        name = str(payload.get("name") or "").strip()
        if not name:
            raise ValueError("模型名不能为空")
        strategy = str(payload.get("strategy") or self.default_strategy).lower()
        if strategy not in VALID_STRATEGIES:
            raise ValueError(f"不支持的策略: {strategy!r}")
        raw_entries = payload.get("entries")
        if not isinstance(raw_entries, list):
            raise ValueError("entries 必须是数组")

        entries: list[RouteEntry] = []
        seen: set[tuple[str, str]] = set()
        for e in raw_entries:
            if not isinstance(e, dict):
                continue
            uid = str(e.get("upstream") or "")
            up = self.find_upstream(uid)
            if up is None:
                continue
            # 同一个上游里的同一个真实模型只留一次；
            # 但同一个上游的不同真实模型可以同时参加一个分组
            real = str(e.get("model") or "").strip()
            if not real:
                continue
            # 上游的 /v1/models 经常报不全，用户手填的名字要认，
            # 并且记进这个上游的模型表，这样分组和勾选状态才自洽
            if real not in up.available_models:
                up.available_models.append(real)
                up.available_models.sort()
            if up.selected_models is not None and real not in up.selected_models:
                up.selected_models.append(real)
            if (uid, real) in seen:
                continue
            seen.add((uid, real))
            entries.append(RouteEntry({
                "upstream": uid,
                "model": real,
                "enabled": e.get("enabled", True),
            }))
        if not entries:
            raise ValueError("至少要给这个分组选一个上游模型")

        self.models[name] = ModelRoute(name, {"strategy": strategy,
                                             "entries": [e.state() for e in entries]})
        self._save_state()
        return self.models[name]

    def delete_model(self, name: str) -> dict:
        if name not in self.models:
            raise KeyError(f"没有配置模型 {name!r}")
        del self.models[name]
        self._save_state()
        return {"ok": True, "deleted": name}

    def auto_configure(self, names: Optional[list[str]] = None,
                       strategy: Optional[str] = None) -> dict:
        """给「你已经勾选过的分组」自动建路由，顺序按上游当前顺序。

        只认勾选，没勾的不会被拉进来。已有的分组不会被覆盖。
        """
        strat = (strategy or self.default_strategy).lower()
        if strat not in VALID_STRATEGIES:
            strat = "priority"

        # 分组名 → [(上游, 真实模型名)]
        by_group: dict[str, list[tuple[Upstream, str]]] = {}
        for up in self.upstreams:
            for real in up.selected():
                by_group.setdefault(up.group_of(real), []).append((up, real))

        wanted = [n for n in (names or by_group.keys()) if n in by_group]
        created: list[str] = []
        skipped: list[str] = []
        for group in sorted(wanted):
            if group in self.models:
                skipped.append(group)
                continue
            entries = [{"upstream": u.id, "model": real, "enabled": True}
                       for u, real in by_group[group]]
            self.models[group] = ModelRoute(group, {"strategy": strat, "entries": entries})
            created.append(group)
        self._save_state()
        return {"ok": True, "created": created, "skipped": skipped}

    # ---- 候选模型总表（给界面用）----
    def model_catalog(self) -> list[dict]:
        """按分组汇总：每个分组有哪些上游参与、各自用什么真实名字。

        另外单独返回「有但没勾选」的真实模型，界面上用来提示还没选用。
        """
        catalog: dict[str, dict] = {}
        order = {u.id: i for i, u in enumerate(self.upstreams)}

        def entry_for(group: str) -> dict:
            return catalog.setdefault(group, {
                "name": group,
                "upstreams": [],       # 参与这个分组的上游（含真实模型名）
                "real_models": [],     # 这个分组下所有真实模型名（去重）
                "offered_only": [],    # 有这些模型但还没勾选的上游
                "configured": False,
                "strategy": None,
            })

        for up in self.upstreams:
            chosen = set(up.selected())
            for real in up.available_models:
                if real in chosen:
                    group = up.group_of(real)
                    e = entry_for(group)
                    e["upstreams"].append({
                        "id": up.id, "name": up.name, "enabled": up.enabled,
                        "model": real,                      # 这个上游用的真实名字
                        "renamed": real != group,           # 是否改过名
                    })
                    if real not in e["real_models"]:
                        e["real_models"].append(real)
                else:
                    # 有，但用户没勾选
                    entry_for(real)["offered_only"].append({"id": up.id, "name": up.name})

        # 配过路由的分组，哪怕上游没勾也要显示，方便手工维护
        for group, route in self.models.items():
            e = entry_for(group)
            e["configured"] = True
            e["strategy"] = route.strategy
            have = {(u["id"], u["model"]) for u in e["upstreams"]}
            for ent in route.entries:
                up = self.find_upstream(ent.upstream_id)
                if up is None:
                    continue
                if (up.id, ent.model) in have:
                    continue
                e["upstreams"].append({
                    "id": up.id, "name": up.name, "enabled": up.enabled,
                    "model": ent.model, "renamed": ent.model != group,
                })
                have.add((up.id, ent.model))
                if ent.model not in e["real_models"]:
                    e["real_models"].append(ent.model)

        for e in catalog.values():
            e["upstreams"].sort(key=lambda u: order.get(u["id"], 999))
            e["offered_only"] = [u for u in e["offered_only"]
                                 if u["id"] not in {x["id"] for x in e["upstreams"]}]
            e["upstream_count"] = len(e["upstreams"])
            e["offered_only_count"] = len(e["offered_only"])
            # 各家名字不一致时，界面上要能一眼看出来
            e["aliased"] = len(set(e["real_models"])) > 1 or (
                bool(e["real_models"]) and e["real_models"][0] != e["name"])

        return sorted(catalog.values(), key=lambda e: e["name"].lower())

    # ---- 请求路由 ----
    def resolve(self, requested_model: Optional[str]) -> tuple[list[tuple[Upstream, str]], str]:
        """返回 ([(上游, 该上游上的模型名)], 路由来源说明)。"""
        model = requested_model or ""
        route = self.models.get(model)

        pairs: list[tuple[Upstream, str]] = []
        if route is not None:
            for e in route.entries:
                if not e.enabled:
                    continue
                up = self.find_upstream(e.upstream_id)
                if up is None or not up.enabled:
                    continue
                pairs.append((up, e.model or model))
            source = "配置"
            if pairs:
                return self._order(pairs, route.strategy, model), source
            # 配了但全被停用/删光，退回自动
            source = "配置为空，自动"

        # 没配过分组：先看有没有上游把这个名字当分组名（勾选过的），
        # 这样「统一叫 deepseek」即使还没建路由也能走通
        by_group: list[tuple[Upstream, str]] = []
        for u in self.upstreams:
            if not u.enabled:
                continue
            reals = u.real_models_in(model)
            for real in reals:
                by_group.append((u, real))
        if by_group:
            return by_group, "自动（按分组）"

        # 再挑「确实拉取到了这个模型」的上游（按上游自己报的清单，不看勾选），
        # 这样至少不会把请求发给一家根本没这个模型的
        auto = [(u, model) for u in self.upstreams if u.enabled and u.is_offered(model)]
        if auto:
            return auto, "自动（按已拉取的模型列表）"

        # 再退一步：所有启用的上游都试，模型名原样透传
        fallback = [(u, model) for u in self.upstreams if u.enabled]
        return fallback, "未配置，按上游顺序兜底"

    def _order(self, pairs: list[tuple[Upstream, str]], strategy: str,
               model: str) -> list[tuple[Upstream, str]]:
        ready = [p for p in pairs if p[0].available()]
        pool = ready or pairs  # 全在冷却中也得试，总比直接失败好

        if strategy == "random":
            return random.sample(pool, len(pool))

        if strategy == "round_robin":
            n = len(pool)
            with self.lock:
                pos = self.rr_counters.get(model, 0) % n
                self.rr_counters[model] = pos + 1
            return pool[pos:] + pool[:pos]

        # priority：按配置顺序，但可用的排前面
        if ready:
            return ready + [p for p in pairs if p not in ready]
        return pool

    # ---- 调用统计（给左侧面板用）----
    def call_stats(self) -> dict:
        """按分组、以及分组×上游 汇总调用次数和 token 用量。

        数据来源是 history（最近 200 条），好处是重启后清空、
        不会和历史记录对不上；坏处是只反映最近这些请求。
        """
        with self.lock:
            hist = list(self.history)

        groups: dict[str, dict] = {}
        for h in hist:
            name = h.get("model") or "-"
            g = groups.setdefault(name, {
                "group": name,
                "calls": 0, "ok": 0, "failed": 0,
                "upstreams": {},
                "usage": {},          # 合计
                "total_elapsed": 0.0,
            })
            g["calls"] += 1
            if h.get("ok"):
                g["ok"] += 1
            else:
                g["failed"] += 1
            g["total_elapsed"] += float(h.get("elapsed_seconds") or 0)

            up_name = h.get("upstream") or "-"
            u = g["upstreams"].setdefault(up_name, {
                "upstream": up_name, "calls": 0, "ok": 0, "failed": 0, "usage": {},
            })
            u["calls"] += 1
            if h.get("ok"):
                u["ok"] += 1
            else:
                u["failed"] += 1

            use = h.get("usage") or {}
            for key in ("input", "output", "total", "cache_read", "cache_write"):
                if use.get(key) is not None:
                    g["usage"][key] = g["usage"].get(key, 0) + int(use[key])
                    u["usage"][key] = u["usage"].get(key, 0) + int(use[key])

        out: list[dict] = []
        for name, g in groups.items():
            ups = sorted(g["upstreams"].values(), key=lambda x: -x["calls"])
            out.append({
                "group": name,
                "calls": g["calls"],
                "ok": g["ok"],
                "failed": g["failed"],
                "avg_elapsed": round(g["total_elapsed"] / g["calls"], 3) if g["calls"] else 0,
                "usage": g["usage"],
                "upstreams": ups,
                "upstream_count": len(ups),
            })
        out.sort(key=lambda x: -x["calls"])

        # 总量
        totals = {"calls": len(hist), "ok": 0, "failed": 0,
                  "input": 0, "output": 0, "total": 0,
                  "cache_read": 0, "cache_write": 0}
        for h in hist:
            if h.get("ok"):
                totals["ok"] += 1
            else:
                totals["failed"] += 1
            use = h.get("usage") or {}
            for key in ("input", "output", "total", "cache_read", "cache_write"):
                if use.get(key) is not None:
                    totals[key] += int(use[key])

        return {"groups": out, "totals": totals, "history_limit": self.history.maxlen}

    # ---- 快照 ----
    def snapshot(self) -> dict:
        with self.lock:
            history = list(self.history)
        return {
            "version": __version__,
            "config_path": os.path.abspath(self.config_path),
            "state_path": os.path.abspath(self._state_path()),
            "uptime_seconds": round(time.time() - self.started_at, 1),
            "total_requests": self.total_requests,
            "failure_threshold": self.failure_threshold,
            "cooldown_seconds": self.cooldown_default,
            "default_strategy": self.default_strategy,
            "history": history,
            "upstreams": [u.snapshot() for u in self.upstreams],
            "models": {
                name: {
                    **route.state(),
                    # 每个成员：上游名 + 那边用的真实模型名
                    "members": [
                        {
                            "upstream": e.upstream_id,
                            "upstream_name": (up.name if (up := self.find_upstream(e.upstream_id))
                                              else e.upstream_id),
                            "model": e.model,
                            "renamed": e.model != name,
                            "enabled": e.enabled,
                        }
                        for e in route.entries
                    ],
                    "upstream_names": [
                        (up.name if (up := self.find_upstream(e.upstream_id)) else e.upstream_id)
                        for e in route.entries
                    ],
                    "real_models": sorted({e.model for e in route.entries}),
                    "resolved_count": len([
                        e for e in route.entries
                        if e.enabled and (u := self.find_upstream(e.upstream_id)) and u.enabled
                    ]),
                }
                for name, route in self.models.items()
            },
            "catalog": self.model_catalog(),
            "call_stats": self.call_stats(),
        }


# ---------------------------------------------------------------------------
# SSE 流式响应里把模型名改回客户端请求的名字
# ---------------------------------------------------------------------------

class SSEModelRewriter:
    def __init__(self, requested: str, effective: str):
        self.requested = requested
        self.effective = effective
        self.buf = b""
        self.need_rewrite = bool(requested) and requested != effective

    def feed(self, data: bytes, final: bool = False) -> bytes:
        if not self.need_rewrite:
            return data
        self.buf += data
        if not final and b"\n" not in self.buf:
            return b""
        if final:
            block, self.buf = self.buf, b""
        else:
            idx = self.buf.rfind(b"\n")
            block, self.buf = self.buf[: idx + 1], self.buf[idx + 1:]
        return self._rewrite(block)

    def _rewrite(self, block: bytes) -> bytes:
        out: list[bytes] = []
        for line in block.split(b"\n"):
            stripped = line.strip()
            if stripped.startswith(b"data:") and b'"model"' in line:
                payload = stripped[5:].strip()
                if payload and payload != b"[DONE]":
                    try:
                        obj = json.loads(payload)
                    except Exception:
                        out.append(line)
                        continue
                    if isinstance(obj, dict) and obj.get("model") == self.effective:
                        obj["model"] = self.requested
                        out.append(b"data: " + json.dumps(
                            obj, ensure_ascii=False, separators=(",", ":")).encode("utf-8"))
                        continue
            out.append(line)
        return b"\n".join(out)


def rewrite_json_model(body: bytes, effective: str, requested: str) -> bytes:
    try:
        obj = json.loads(body)
    except Exception:
        return body
    if isinstance(obj, dict) and obj.get("model") == effective:
        obj["model"] = requested
        try:
            return json.dumps(obj, ensure_ascii=False).encode("utf-8")
        except Exception:
            return body
    return body


# ---------------------------------------------------------------------------
# HTTP 处理
# ---------------------------------------------------------------------------

class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    server_version = f"llm-router/{__version__}"
    sys_version = ""

    @property
    def router(self) -> Router:
        return self.server.router  # type: ignore[attr-defined]

    def setup(self) -> None:
        super().setup()
        try:
            self.connection.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        except OSError:
            pass

    def log_message(self, fmt: str, *args: Any) -> None:  # noqa: A003
        if self.router.log_requests:
            sys.stderr.write("[%s] %s\n" % (time.strftime("%H:%M:%S"), fmt % args))

    def send_header(self, keyword: str, value: Any) -> None:
        """统一把响应头值压成 ASCII，避免中文上游名把响应搞坏。"""
        super().send_header(keyword, header_value(value))

    def handle_one_request(self) -> None:
        """把客户端断开当成正常事件，别刷一堆 traceback。"""
        try:
            super().handle_one_request()
        except (BrokenPipeError, ConnectionResetError, ConnectionAbortedError):
            self.close_connection = True

    # ---- 方法分发 ----
    def do_GET(self) -> None:  # noqa: N802
        self._dispatch("GET")

    def do_POST(self) -> None:  # noqa: N802
        self._dispatch("POST")

    def do_PUT(self) -> None:  # noqa: N802
        self._dispatch("PUT")

    def do_PATCH(self) -> None:  # noqa: N802
        self._dispatch("PATCH")

    def do_DELETE(self) -> None:  # noqa: N802
        self._dispatch("DELETE")

    def do_OPTIONS(self) -> None:  # noqa: N802
        self.send_response(204)
        self._cors_headers()
        self.send_header("Content-Length", "0")
        self.end_headers()

    def _dispatch(self, method: str) -> None:
        path = urllib.parse.urlsplit(self.path).path.rstrip("/") or "/"
        try:
            if path in ("/", "/ui", "/webui", "/webui.html"):
                return self._serve_webui()
            if path in ("/health", "/healthz"):
                return self._send_json(200, {"status": "ok", "version": __version__})
            if path == "/favicon.ico":
                self.send_response(204)
                self.send_header("Content-Length", "0")
                self.end_headers()
                return

            if path in ("/stats", "/admin/stats"):
                return self._send_json(200, self.router.snapshot())
            if path == "/admin/history":
                return self._send_json(200, {"history": list(self.router.history)})

            handlers = {
                "/admin/upstream/add": self._h_upstream_add,
                "/admin/upstream/update": self._h_upstream_update,
                "/admin/upstream/delete": self._h_upstream_delete,
                "/admin/fetch-models": self._h_fetch_models,
                "/admin/select-models": self._h_select_models,
                "/admin/set-group": self._h_set_group,
                "/admin/rename-group": self._h_rename_group,
                "/admin/model/save": self._h_model_save,
                "/admin/model/delete": self._h_model_delete,
                "/admin/auto-configure": self._h_auto_configure,
                "/admin/speedtest": self._h_speedtest,
                "/admin/reset": self._h_reset,
                "/admin/reset-to-config": self._h_reset_to_config,
            }
            if path in handlers:
                if method != "POST":
                    return self._send_json(405, {"error": {"message": f"{path} 需要 POST"}})
                return handlers[path]()

            if path.startswith("/v1/"):
                if path == "/v1/models" and method == "GET":
                    return self._handle_models()
                return self._proxy(method, path)
            return self._send_json(404, {"error": {"message": f"未知路径 {path}"}})
        except BrokenPipeError:
            self.close_connection = True
        except Exception as exc:  # noqa: BLE001
            self.close_connection = True
            try:
                self._send_json(500, {"error": {"message": f"{type(exc).__name__}: {exc}",
                                                "type": "router_error"}})
            except Exception:
                pass

    def _cors_headers(self) -> None:
        if self.router.enable_cors:
            self.send_header("Access-Control-Allow-Origin", "*")
            self.send_header("Access-Control-Allow-Headers", "*")
            self.send_header("Access-Control-Allow-Methods", "GET, POST, OPTIONS")

    def _send_json(self, status: int, obj: Any) -> None:
        body = json.dumps(obj, ensure_ascii=False, indent=2).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self._cors_headers()
        self.end_headers()
        self.wfile.write(body)

    def _serve_webui(self) -> None:
        path = os.path.join(os.path.dirname(os.path.abspath(__file__)), WEBUI_FILENAME)
        try:
            with open(path, "rb") as fh:
                body = fh.read()
        except FileNotFoundError:
            return self._send_json(404, {"error": {
                "message": f"找不到 {WEBUI_FILENAME}，请把它和 router.py 放在同一目录"}})
        self.send_response(200)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(body)

    # ---- 读请求体 ----
    def _read_body(self) -> bytes:
        limit = int(self.router.max_body_mb * 1024 * 1024)
        te = (self.headers.get("Transfer-Encoding") or "").lower()
        if "chunked" in te:
            return self._read_chunked_body(limit)
        cl = self.headers.get("Content-Length")
        if not cl:
            return b""
        try:
            remaining = int(cl)
        except ValueError:
            return b""
        if remaining <= 0:
            return b""
        if remaining > limit:
            raise ValueError(f"请求体超过上限 {self.router.max_body_mb} MB")
        chunks: list[bytes] = []
        while remaining > 0:
            piece = self.rfile.read(min(remaining, 262144))
            if not piece:
                break
            chunks.append(piece)
            remaining -= len(piece)
        return b"".join(chunks)

    def _read_chunked_body(self, limit: int) -> bytes:
        out = bytearray()
        while True:
            line = self.rfile.readline(65536)
            if not line:
                break
            line = line.strip()
            if not line:
                continue
            try:
                size = int(line.split(b";")[0], 16)
            except ValueError:
                break
            if size == 0:
                while True:
                    trailer = self.rfile.readline(65536)
                    if trailer in (b"\r\n", b"\n", b""):
                        break
                break
            out += self.rfile.read(size)
            self.rfile.read(2)
            if len(out) > limit:
                raise ValueError(f"请求体超过上限 {self.router.max_body_mb} MB")
        return bytes(out)

    def _read_json_body(self) -> dict:
        raw = self._read_body()
        if not raw:
            return {}
        try:
            data = json.loads(raw)
        except Exception as exc:  # noqa: BLE001
            raise ValueError(f"请求体不是合法 JSON: {exc}") from exc
        if not isinstance(data, dict):
            raise ValueError("请求体必须是 JSON 对象")
        return data

    # ---- /v1/models ----
    def _handle_models(self) -> None:
        """只列出「已选用」的模型，也就是客户端真正能调的。"""
        now = int(time.time())
        data = []
        for entry in self.router.model_catalog():
            if not entry["upstream_count"]:
                continue  # 一个上游都没勾选，就不对外暴露
            data.append({
                "id": entry["name"],
                "object": "model",
                "created": now,
                "owned_by": "llm-router",
                "upstreams": [u["name"] for u in entry["upstreams"]],
                "configured": entry["configured"],
            })
        if not data:
            first = next((u for u in self.router.upstreams if u.enabled), None)
            if first is None:
                return self._send_json(200, {"object": "list", "data": []})
            # 还没勾选任何模型，透传问上游，让客户端至少能看到点东西
            return self._proxy("GET", "/v1/models")
        return self._send_json(200, {"object": "list", "data": data})

    # ---- 管理接口 ----
    @staticmethod
    def _result(fn):
        """跑一个管理操作，统一成 (状态码, 响应体)。

        注意成功时也要包成 (200, 结果)，否则调用方 `status, out = ...`
        会把返回的 dict 当元组拆包，把第一个键名当成状态码。
        """
        try:
            return 200, fn()
        except KeyError as exc:
            return 404, {"ok": False, "error": str(exc).strip("'\"")}
        except ValueError as exc:
            return 400, {"ok": False, "error": str(exc)}
        except Exception as exc:  # noqa: BLE001
            return 500, {"ok": False, "error": f"{type(exc).__name__}: {exc}"}

    def _h_upstream_add(self) -> None:
        status, out = self._result(lambda: {
            "ok": True, "upstream": self.router.add_upstream(self._read_json_body()).snapshot()})
        self._send_json(status, out)

    def _h_upstream_update(self) -> None:
        status, out = self._result(lambda: {
            "ok": True, "upstream": self.router.update_upstream(self._read_json_body()).snapshot()})
        self._send_json(status, out)

    def _h_upstream_delete(self) -> None:
        def go():
            payload = self._read_json_body()
            out = self.router.delete_upstream(str(payload.get("id") or ""))
            return out
        status, out = self._result(go)
        self._send_json(status, out)

    def _h_fetch_models(self) -> None:
        def go():
            payload = self._read_json_body()
            ids = payload.get("ids")
            if ids is not None and not isinstance(ids, list):
                raise ValueError("ids 必须是数组")
            return {"ok": True, "results": self.router.fetch_models(
                [str(i) for i in ids] if ids else None)}
        status, out = self._result(go)
        self._send_json(status, out)

    def _h_model_save(self) -> None:
        def go():
            route = self.router.save_model(self._read_json_body())
            return {"ok": True, "model": {"name": route.name, **route.state()}}
        status, out = self._result(go)
        self._send_json(status, out)

    def _h_model_delete(self) -> None:
        def go():
            payload = self._read_json_body()
            return self.router.delete_model(str(payload.get("name") or ""))
        status, out = self._result(go)
        self._send_json(status, out)

    def _h_auto_configure(self) -> None:
        def go():
            payload = self._read_json_body()
            names = payload.get("names")
            if names is not None and not isinstance(names, list):
                raise ValueError("names 必须是数组")
            return self.router.auto_configure(
                [str(n) for n in names] if names else None, payload.get("strategy"))
        status, out = self._result(go)
        self._send_json(status, out)

    def _h_select_models(self) -> None:
        def go():
            payload = self._read_json_body()
            uid = str(payload.get("id") or "")
            if payload.get("all") is True:
                return self.router.select_all_models(uid)
            if payload.get("none") is True:
                return self.router.select_no_models(uid)
            models = payload.get("models")
            if not isinstance(models, list):
                raise ValueError("models 必须是数组（或者用 all/none）")
            return self.router.apply_selected_models(uid, [str(m) for m in models])
        status, out = self._result(go)
        self._send_json(status, out)

    def _h_set_group(self) -> None:
        """设置「上游真实模型名 → 分组名」的映射。"""
        def go():
            payload = self._read_json_body()
            uid = str(payload.get("id") or "")
            mapping = payload.get("mapping")
            if not isinstance(mapping, dict):
                raise ValueError("mapping 必须是对象，形如 {\"ds1\": \"deepseek\"}")
            return self.router.set_model_group(uid, mapping)
        status, out = self._result(go)
        self._send_json(status, out)

    def _h_rename_group(self) -> None:
        def go():
            payload = self._read_json_body()
            return self.router.rename_group(str(payload.get("from") or ""),
                                            str(payload.get("to") or ""))
        status, out = self._result(go)
        self._send_json(status, out)

    def _h_speedtest(self) -> None:
        def go():
            payload = self._read_json_body()
            ids = payload.get("ids")
            if ids is not None and not isinstance(ids, list):
                raise ValueError("ids 必须是数组")
            targets = [u for u in self.router.upstreams
                       if u.enabled and (not ids or u.id in set(str(i) for i in ids))]
            if not targets:
                return {"ok": True, "results": []}
            results: list[dict] = []
            with concurrent.futures.ThreadPoolExecutor(
                    max_workers=min(8, len(targets))) as pool:
                futures = [pool.submit(u.ping) for u in targets]
                for fut in concurrent.futures.as_completed(futures, timeout=90):
                    try:
                        results.append(fut.result())
                    except Exception as exc:  # noqa: BLE001
                        results.append({"ok": False, "error": str(exc), "latency_seconds": None})
            order = {u.id: i for i, u in enumerate(self.router.upstreams)}
            results.sort(key=lambda r: order.get(r.get("id", ""), 999))
            return {"ok": True, "results": results}
        status, out = self._result(go)
        self._send_json(status, out)

    def _h_reset(self) -> None:
        for u in self.router.upstreams:
            u.reset_stats()
        with self.router.lock:
            self.router.history.clear()
            self.router.total_requests = 0
            self.router.rr_counters.clear()
        self._send_json(200, {"ok": True, "message": "统计与冷却已清零"})

    def _h_reset_to_config(self) -> None:
        status, out = self._result(self.router.reset_to_config)
        self._send_json(status, out)

    # ---- 历史记录 ----
    def _record_history(self, upstream: Optional[Upstream], requested_model: Optional[str],
                        effective_model: str, status: int, elapsed: float,
                        error: Optional[str] = None, stream: bool = False,
                        attempts: int = 1, route_source: str = "",
                        usage: Optional[dict] = None,
                        request_text: Optional[str] = None,
                        response_text: Optional[str] = None,
                        reasoning_text: Optional[str] = None) -> None:
        entry = {
            "at": time.time(),
            "time": time.strftime("%H:%M:%S"),
            "upstream_id": upstream.id if upstream else None,
            "upstream": upstream.name if upstream else "-",
            "model": requested_model or "-",
            "effective_model": effective_model if effective_model not in ("-", None) else (requested_model or "-"),
            "status": status,
            "elapsed_seconds": round(elapsed, 3),
            "ok": 200 <= status < 400,
            "stream": stream,
            "attempts": attempts,
            "route_source": route_source,
            "error": error,
            # token 用量：输入 / 输出 / 缓存命中 / 缓存写入 / 合计
            "usage": usage or {},
            # 输入与输出内容（截断保存，够看就行，不无限涨内存）
            "request_text": (request_text or "")[:HISTORY_TEXT_LIMIT] or None,
            "response_text": (response_text or "")[:HISTORY_TEXT_LIMIT] or None,
            # 推理模型的思考过程（DeepSeek-R1 这类会单独吐 reasoning_content）
            "reasoning_text": (reasoning_text or "")[:HISTORY_TEXT_LIMIT] or None,
        }
        with self.router.lock:
            self.router.history.append(entry)
            self.router.total_requests += 1

    # ---- 核心代理 ----
    def _proxy(self, method: str, path: str) -> None:
        raw_body = self._read_body() if method in ("POST", "PUT", "PATCH") else b""

        payload: Optional[dict] = None
        requested_model: Optional[str] = None
        if raw_body:
            try:
                parsed = json.loads(raw_body)
                if isinstance(parsed, dict):
                    payload = parsed
                    m = parsed.get("model")
                    requested_model = str(m) if m else None
            except Exception:
                payload = None

        pairs, route_source = self.router.resolve(requested_model)
        if not pairs:
            self._record_history(None, requested_model, "-", 503, 0.0, "没有可用的上游")
            return self._send_json(503, {"error": {
                "message": (f"没有可用的上游来处理 model={requested_model!r}；"
                            f"请先在网页控制台里添加上游并配置这个模型"),
                "type": "no_available_upstream"}})

        t_start = time.time()
        last: Optional[Retryable] = None
        attempts_log: list[tuple[Upstream, str]] = []
        for i, (up, eff_model) in enumerate(pairs):
            try:
                self._try_upstream(up, eff_model, method, path, payload, raw_body,
                                   requested_model, route_source, attempts=len(pairs))
                return
            except Retryable as exc:
                last = exc
                attempts_log.append((up, exc.reason))
                remaining = len(pairs) - i - 1
                if remaining > 0:
                    self.log_message("上游 %s 失败(%s)，换下一个（还剩 %d 个）",
                                     up.name, exc.reason, remaining)
                continue

        if last is not None:
            self._record_history(last.upstream, requested_model, "-", last.status,
                                 time.time() - t_start, last.reason,
                                 attempts=len(pairs), route_source=route_source)
            self.close_connection = False
            body = last.body
            try:
                json.loads(body)
            except Exception:
                body = json.dumps({"error": {
                    "message": last.reason or "上游不可用", "type": "upstream_error",
                    "upstream": last.upstream.name}}, ensure_ascii=False).encode("utf-8")
            self.send_response(last.status if 400 <= last.status < 600 else 502)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.send_header("X-Upstream", last.upstream.name)
            self.send_header("X-Upstream-Attempted",
                             ",".join(header_value(u.name) for u, _ in pairs))
            self.send_header("X-Upstream-Errors",
                             "; ".join(f"{header_value(u.name)}={header_value(r)}"
                                       for u, r in attempts_log) or "-")
            self._cors_headers()
            self.end_headers()
            self.wfile.write(body)

    def _try_upstream(self, up: Upstream, eff_model: str, method: str, path: str,
                      payload: Optional[dict], raw_body: bytes,
                      requested_model: Optional[str], route_source: str,
                      attempts: int) -> None:
        data = raw_body or None
        if payload is not None and requested_model and eff_model != requested_model:
            new_payload = dict(payload)
            new_payload["model"] = eff_model
            data = json.dumps(new_payload, ensure_ascii=False).encode("utf-8")

        headers = up.headers(
            content_type=self.headers.get("Content-Type") or "application/json",
            accept=self.headers.get("Accept") or "application/json",
        )
        conn = None
        t0 = time.time()
        try:
            conn, resp, _ = up.open(method, path, data=data, headers=headers)
        except Exception as exc:  # noqa: BLE001
            reason = f"连接失败: {type(exc).__name__}: {exc}"
            up.record_failure(reason, self.router.failure_threshold, up.cooldown)
            raise Retryable(up, 502, json.dumps({"error": {
                "message": reason, "type": "upstream_connect_error",
                "upstream": up.name}}, ensure_ascii=False).encode("utf-8"), reason) from exc

        status = resp.status
        ctype = (resp.getheader("Content-Type") or "").lower()

        # 1) 该换下一个上游的错误
        if status >= 400 and (status in DEFAULT_RETRY_STATUS or status >= 500):
            try:
                body = resp.read()
            except Exception:  # noqa: BLE001
                body = b""
            finally:
                up._close(conn)
            reason = f"HTTP {status}"
            up.record_failure(reason, self.router.failure_threshold, up.cooldown)
            raise Retryable(up, status, body, reason)

        # 2) 客户端自己的错误：原样透传，不算上游的锅
        if status >= 400:
            try:
                body = resp.read()
            except Exception:  # noqa: BLE001
                body = b""
            up._close(conn)
            up.record_neutral(f"HTTP {status}")
            self._relay_json(status, resp, body, up.name, time.time() - t0)
            self._record_history(up, requested_model, eff_model or "-", status,
                                 time.time() - t0, f"客户端错误 HTTP {status}",
                                 attempts=attempts, route_source=route_source)
            return

        # 3) 流式
        if "text/event-stream" in ctype:
            self._relay_stream(conn, resp, up, requested_model, eff_model, t0,
                               route_source, attempts, payload)
            return

        # 4) 非流式
        try:
            body = resp.read()
        except Exception as exc:  # noqa: BLE001
            up._close(conn)
            reason = f"读取响应失败: {exc}"
            up.record_failure(reason, self.router.failure_threshold, up.cooldown)
            raise Retryable(up, 502, b"", reason) from exc
        latency = time.time() - t0
        up._close(conn)

        if eff_model and requested_model and eff_model != requested_model:
            body = rewrite_json_model(body, eff_model, requested_model)

        resp_text, usage, reasoning = response_to_text(body)
        up.record_success(latency)
        self._relay_json(status, resp, body, up.name, latency=latency, effective=eff_model)
        self._record_history(up, requested_model, eff_model or "-", status, latency,
                             attempts=attempts, route_source=route_source,
                             usage=usage,
                             request_text=messages_to_text(payload),
                             response_text=resp_text,
                             reasoning_text=reasoning)

    def _relay_json(self, status: int, resp, body: bytes, upstream_name: str,
                    latency: float = 0.0, effective: Optional[str] = None) -> None:
        self.send_response(status)
        self.send_header("Content-Type", resp.getheader("Content-Type") or "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("X-Upstream", upstream_name)
        self.send_header("X-Upstream-Latency", f"{latency:.3f}")
        if effective:
            self.send_header("X-Upstream-Model", effective)
        retry_after = resp.getheader("Retry-After")
        if retry_after:
            self.send_header("Retry-After", retry_after)
        self._cors_headers()
        self.end_headers()
        self.wfile.write(body)

    def _relay_stream(self, conn, resp, up: Upstream, requested_model: Optional[str],
                      eff_model: Optional[str], t0: float, route_source: str,
                      attempts: int, payload: Optional[dict] = None) -> None:
        headers_sent = False
        acc = make_sse_collector()      # 边转发边攒文本和 token 用量
        try:
            rewriter = SSEModelRewriter(requested_model or "", eff_model or "")

            # 先偷看第一块：有些上游返回 200 后立刻断流，
            # 提前拿到首块才能判定失败并切换，而不是把空流甩给客户端。
            first_chunk = b""
            while True:
                piece = resp.read1(65536)
                if not piece:
                    break
                first_chunk = piece
                break
            if not first_chunk:
                up._close(conn)
                reason = "上游返回 200 但流为空"
                up.record_failure(reason, self.router.failure_threshold, up.cooldown)
                raise Retryable(up, 502, b"", reason)

            first_byte_at = time.time()
            self.send_response(resp.status)
            self.send_header("Content-Type", resp.getheader("Content-Type") or "text/event-stream")
            self.send_header("Cache-Control", "no-cache")
            self.send_header("Connection", "keep-alive")
            self.send_header("X-Accel-Buffering", "no")
            self.send_header("X-Upstream", up.name)
            if eff_model:
                self.send_header("X-Upstream-Model", eff_model)
            self.send_header("Transfer-Encoding", "chunked")
            self._cors_headers()
            self.end_headers()
            headers_sent = True

            collect_sse(first_chunk, acc)
            out = rewriter.feed(first_chunk)
            if out:
                self._write_chunk(out)

            while True:
                chunk = resp.read1(262144)
                if not chunk:
                    break
                collect_sse(chunk, acc)
                out = rewriter.feed(chunk)
                if out:
                    self._write_chunk(out)
            tail = rewriter.feed(b"", final=True)
            if tail:
                self._write_chunk(tail)
            self.wfile.write(b"0\r\n\r\n")
            self.wfile.flush()

            latency = first_byte_at - t0
            up.record_success(latency)
            self._record_history(up, requested_model, eff_model or "-", resp.status,
                                 latency, stream=True, attempts=attempts,
                                 route_source=route_source,
                                 usage=acc["usage"],
                                 request_text=messages_to_text(payload),
                                 response_text="".join(acc["text"]),
                                 reasoning_text="".join(acc["reasoning"]))
            self.log_message("流式：上游=%s 首字节=%.2fs 模型=%s",
                             up.name, latency, eff_model or requested_model)
        except Retryable:
            raise
        except (BrokenPipeError, ConnectionResetError):
            up.record_neutral("客户端提前断开")
            self.close_connection = True
        except Exception as exc:  # noqa: BLE001
            up.record_failure(f"流中断: {type(exc).__name__}: {exc}",
                              self.router.failure_threshold, up.cooldown)
            self.close_connection = True
            if not headers_sent:
                raise Retryable(up, 502, b"", f"流式响应失败: {exc}") from exc
        finally:
            up._close(conn)

    def _write_chunk(self, data: bytes) -> None:
        self.wfile.write(b"%x\r\n" % len(data))
        self.wfile.write(data)
        self.wfile.write(b"\r\n")
        self.wfile.flush()  # 必须立即 flush，否则就没有流式效果了


# ---------------------------------------------------------------------------
# 启动
# ---------------------------------------------------------------------------

def print_banner(router: Router) -> None:
    ui = "http://%s:%d/" % (router.host, router.port)
    print("=" * 74)
    print(f"llm-router v{__version__}   上游 {len(router.upstreams)} 个，"
          f"已配置模型 {len(router.models)} 个")
    print(f"网页控制台: {ui}   ← 打开这个")
    print(f"接口地址:   http://{router.host}:{router.port}/v1")
    print("-" * 74)
    if router.upstreams:
        print(f"{'上游':<24}{'状态':<8}{'模型数':<8}{'地址'}")
        for u in router.upstreams:
            state = "启用" if u.enabled else "停用"
            print(f"{u.name:<24}{state:<8}{len(u.available_models):<8}{u.base_url}")
    else:
        print("还没有上游 —— 打开网页控制台添加，或编辑配置文件里的 [[upstreams]]")
    print("-" * 74)
    if router.models:
        for name, route in list(router.models.items())[:12]:
            ups = []
            for e in route.entries:
                up = router.find_upstream(e.upstream_id)
                tag = "" if e.enabled else "(停用)"
                ups.append((up.name if up else e.upstream_id) + tag)
            print(f"  {name:<28} [{route.strategy}] " + " → ".join(ups))
        if len(router.models) > 12:
            print(f"  … 还有 {len(router.models) - 12} 个模型")
    else:
        print("还没有配置模型路由 —— 在网页上点「拉取模型」然后按模型配顺序")
    print("=" * 74)


def main(argv: Optional[list[str]] = None) -> int:
    ap = argparse.ArgumentParser(description="多上游 LLM API 路由代理（按模型配置调用顺序）")
    ap.add_argument("-c", "--config", default="config.toml", help="配置文件路径（TOML）")
    ap.add_argument("--host", help="覆盖配置里的监听地址")
    ap.add_argument("--port", type=int, help="覆盖配置里的监听端口")
    ap.add_argument("--check", action="store_true", help="只校验配置并打印，不启动服务")
    args = ap.parse_args(argv)

    try:
        with open(args.config, "rb") as fh:
            cfg = tomllib.load(fh)
    except FileNotFoundError:
        print(f"找不到配置文件: {args.config}", file=sys.stderr)
        return 2
    except Exception as exc:  # noqa: BLE001
        print(f"配置文件读不了: {exc}", file=sys.stderr)
        return 2

    try:
        router = Router(cfg, args.config)
    except Exception as exc:  # noqa: BLE001
        print(f"配置有问题: {exc}", file=sys.stderr)
        return 2

    if args.host:
        router.host = args.host
    if args.port:
        router.port = args.port

    print_banner(router)
    if args.check:
        return 0

    httpd = ThreadingHTTPServer((router.host, router.port), Handler)
    httpd.daemon_threads = True
    httpd.router = router  # type: ignore[attr-defined]

    print("\n服务已启动，Ctrl+C 停止。")
    print(f"  客户端 base_url 用: http://{router.host}:{router.port}/v1\n")
    try:
        httpd.serve_forever()
    except KeyboardInterrupt:
        print("\n正在停止…")
    finally:
        httpd.server_close()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
