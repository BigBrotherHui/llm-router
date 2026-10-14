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

__version__ = "2.4.1"

WEBUI_FILENAME = "webui.html"

# 判断「上游是不是把网页当 API 响应发回来了」。
# 不少网关（New API / one-api 这类）对认不出的路径不报 404，而是回 200 + 首页 HTML。
# 一旦把它当成功，后面就是「测试永远通、调用永远断」：客户端要的是 SSE，
# 收到一坨 HTML 直接解析失败。所以 2xx + HTML 一律按失败处理。
HTML_CT_MARKERS = ("text/html", "application/xhtml")
HTML_BODY_PREFIXES = (b"<!doctype html", b"<html")


def looks_like_html_page(ctype: str, body: bytes = b"") -> bool:
    """Content-Type 或正文开头像网页，就认为这不是 API 响应。"""
    ct = (ctype or "").lower()
    if any(m in ct for m in HTML_CT_MARKERS):
        return True
    head = body[:200].lstrip().lower()
    return any(head.startswith(p) for p in HTML_BODY_PREFIXES)


def _looks_like_sse_head(head: bytes) -> bool:
    """正文开头是不是 SSE 事件（用来识别标错 Content-Type 的流式响应）。"""
    h = (head or b"").lstrip()
    if not h:
        return False
    first = h.split(b"\n", 1)[0].strip()
    return first.startswith((b"data:", b"event:"))

# 组里的上游全部因「并发/速率已满」被跳过时，最多等这么久碰运气（秒）。
# 0 = 不等，直接回 429。等待只在**自己配的这些上游**之间排队，
# 不会让请求无限期挂着。
QUEUE_TIMEOUT_DEFAULT = 15.0

# 状态文件的历史版本保留份数（每次保存都会轮转出一份带时间戳的快照）。
# 之前只有单份 .bak，结果「最后一次写入」会把上一版好配置挤掉，
# 出了事就没有退路，所以改成留多份。
STATE_BACKUP_KEEP = 20

# 管理接口的审计记录：谁在什么时候改了什么，写到
# .llmrouter/<配置名>.admin.log（跟着配置文件名走，多份配置不会混在一起）。
# 配置被莫名改动时，靠这个能查出是哪次操作、来自哪个地址。
ADMIN_LOG_SUFFIX = ".admin.log"

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


def response_to_text(body: bytes) -> tuple[str, dict, str]:
    """从非流式响应里取出回复文本、token 用量和推理内容。

    返回元数必须和调用方的解包一致（三个）。上游返回非 JSON 内容时
    少给一个，会把本来正常的请求打成 500。
    """
    try:
        obj = json.loads(body)
    except Exception:
        return "", {}, ""
    if not isinstance(obj, dict):
        return "", {}, ""

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
# 上游容量闸门（并发上限 + 每分钟速率上限）
# ---------------------------------------------------------------------------

class CapacityGate:
    """一个上游的「同时能跑几个 / 每分钟能发几个」闸门。

    为什么放在路由器这一层而不是靠上游自己 429：
    同一个厂商开了好几个账号分摊额度时，上游只知道「这个 Key 超了」，
    它会直接拒绝，而路由器手里有整个组的信息 —— 可以先挑还有容量的那家发，
    从源头上少撞 429。这里只管**自己这边的上限**（用户按上游手填的），
    不做全局调度，也不猜上游的真实配额。

    速率用的是**最近 60 秒滑动窗口**（不是「每分钟整点清零」），
    否则整点那一瞬间会双倍放行。
    """

    WINDOW = 60.0
    # 滑动窗口只留时间戳，超过这个数量就没必要记得更细了。
    # 免得「不限速率」的上游被无限追加时间戳把内存撑起来。
    MAX_STAMPS = 4096

    def __init__(self) -> None:
        self.lock = threading.Lock()
        self.in_flight: int = 0
        self._stamps: collections.deque = collections.deque()

    # ---- 配置（存在 Upstream 上，这里只读）----
    def _trim(self, now: float) -> None:
        cutoff = now - self.WINDOW
        stamps = self._stamps
        while stamps and stamps[0] <= cutoff:
            stamps.popleft()

    def used_last_minute(self, now: Optional[float] = None) -> int:
        now = time.time() if now is None else now
        with self.lock:
            self._trim(now)
            return len(self._stamps)

    def acquire(self, max_concurrent: Optional[int],
                max_per_minute: Optional[int],
                wait: float = 0.0) -> tuple[bool, str]:
        """尝试占一个坑。返回 (是否放行, 不放行的原因)。

        wait > 0 时会在「并发满了」的情况下小睡重试，直到超时。
        速率超限**不等待** —— 那要等到窗口滑出去，等一个请求的时间
        通常已经超过客户端的超时了，不如立刻换下一家。
        """
        deadline = time.time() + max(0.0, wait)
        while True:
            now = time.time()
            with self.lock:
                self._trim(now)
                if max_per_minute is not None and len(self._stamps) >= max_per_minute:
                    return False, f"每分钟上限 {max_per_minute} 已用满"
                if max_concurrent is not None and self.in_flight >= max_concurrent:
                    full = True
                else:
                    full = False
                    self.in_flight += 1
                    # 发出去的这一发也计入速率窗口
                    if len(self._stamps) < self.MAX_STAMPS:
                        self._stamps.append(now)
                    return True, ""
            if not full or time.time() >= deadline:
                return False, f"并发上限 {max_concurrent} 已满"
            time.sleep(0.05)

    def release(self) -> None:
        with self.lock:
            if self.in_flight > 0:
                self.in_flight -= 1

    def state(self, max_concurrent: Optional[int], max_per_minute: Optional[int]) -> dict:
        now = time.time()
        with self.lock:
            self._trim(now)
            used = len(self._stamps)
            inflight = self.in_flight
        return {
            "in_flight": inflight,
            "used_last_minute": used,
            "max_concurrent": max_concurrent,
            "max_per_minute": max_per_minute,
            # 还有没有容量（给前端标「已满」用；满了不代表坏了）
            "has_room": (
                (max_concurrent is None or inflight < max_concurrent)
                and (max_per_minute is None or used < max_per_minute)
            ),
        }


class CapacityLease:
    """占坑的凭据。用 with 包住，异常路径也不会漏掉 release。"""

    __slots__ = ("gate", "released")

    def __init__(self, gate: Optional[CapacityGate]):
        self.gate = gate
        self.released = False

    def release(self) -> None:
        if self.gate is not None and not self.released:
            self.released = True
            self.gate.release()

    def __enter__(self) -> "CapacityLease":
        return self

    def __exit__(self, *exc: Any) -> None:
        self.release()


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
        # 最近一次连通性测试结果(持久化，重启后还在)
        self.last_check: Optional[dict] = cfg.get("last_check")
        # 每个模型各测一次的结果(model -> {ok/latency/at/...})，持久化
        raw_checks = cfg.get("last_checks")
        self.last_checks: dict[str, dict] = (
            {str(k): v for k, v in raw_checks.items()} if isinstance(raw_checks, dict) else {}
        )

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

        # 容量上限（用户按上游手填）。None = 不限。
        # max_concurrent 是「同时在跑几个请求」，max_per_minute 是「60 秒内发几次」。
        # 只影响**发往这家**的请求：满了就换下一家，而不是让请求失败。
        self.max_concurrent: Optional[int] = self._pos_int(cfg.get("max_concurrent"))
        self.max_per_minute: Optional[int] = self._pos_int(cfg.get("max_per_minute"))
        self.gate = CapacityGate()

    @staticmethod
    def _pos_int(value: Any) -> Optional[int]:
        """把配置里的上限读成正整数；空/0/负数/看不懂的一律当「不限」。"""
        if value is None or value == "":
            return None
        try:
            n = int(value)
        except (TypeError, ValueError):
            return None
        return n if n > 0 else None

    # ---- 可用性 ----
    def available(self) -> bool:
        return self.enabled and time.time() >= self.cooldown_until

    def cooling_remaining(self) -> float:
        return max(0.0, self.cooldown_until - time.time())

    # ---- 容量 ----
    def has_capacity_limit(self) -> bool:
        return self.max_concurrent is not None or self.max_per_minute is not None

    def capacity_state(self) -> dict:
        return self.gate.state(self.max_concurrent, self.max_per_minute)

    def acquire(self, wait: float = 0.0) -> tuple[CapacityLease, str]:
        """占一个坑。返回 (凭据, 失败原因)；凭据的 gate 为 None 表示被挡下了。

        没配任何上限的上游不占坑（凭据是空壳），零开销。
        """
        if not self.has_capacity_limit():
            return CapacityLease(None), ""
        ok, why = self.gate.acquire(self.max_concurrent, self.max_per_minute, wait)
        return CapacityLease(self.gate if ok else None), ("" if ok else why)


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

    def _remember_check(self, result: dict) -> None:
        """记下最近一次连通性测试结果，随状态持久化。

        按模型分别记(last_checks[model])：一个上游可能勾了多个模型，
        每个模型的连通性/延迟各不一样，要能分别看到。
        """
        entry = {
            "at": result.get("at") or time.time(),
            "ok": bool(result.get("ok")),
            "status": result.get("status"),
            "latency_seconds": result.get("latency_seconds"),
            "error": result.get("error"),
            "rate_limited": bool(result.get("rate_limited")),
            "model": result.get("model"),
        }
        with self._lock:
            self.last_check = entry
            m = entry.get("model")
            if m:
                self.last_checks[str(m)] = entry

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
                "last_check": self.last_check,
                "last_checks": dict(self.last_checks),
                # 容量：配了多少、现在用了多少（前端卡片要显示实时占用）
                "capacity": self.capacity_state(),
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
            "last_check": self.last_check,
            "last_checks": dict(self.last_checks),
            # 只在上限设过时才写，保持老状态文件干净
            **({"max_concurrent": self.max_concurrent} if self.max_concurrent else {}),
            **({"max_per_minute": self.max_per_minute} if self.max_per_minute else {}),
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
                    # 万一全都不行，至少能把这个 404 原样透传出去。
                    # 之前攒下的那条要关掉，否则换候选时连接会泄漏。
                    if last_resp is not None:
                        self._close(last_resp[0])
                    last_resp = (conn, resp, target, suffix)
                    continue
                if looks_like_html_page(resp.getheader("Content-Type") or ""):
                    # 200 + 网页：这个地址根本没有 API（网关把首页兜底返回了），
                    # 不能算成功，更不能把它记成「可用拼法」—— 否则后面每个
                    # 请求都会优先打这个网页地址，表现为「测试通、调用必断」。
                    # 留着响应，万一别的候选也不行，至少能把它透传出去。
                    if last_resp is not None:
                        self._close(last_resp[0])
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
            # 走到这里说明所有候选都没成功。若手上这个只是网页（不是 404），
            # 不要把它记成可用拼法；404 则可以记 —— 至少路径形状对，
            # 下次还是先试它，能省一次往返。
            if resp.status == 404:
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

            if looks_like_html_page(resp.getheader("Content-Type") or "", body):
                # 200 + 网页：地址/路径其实不对，网关拿首页兜底了。
                # 直接说清楚，比「不是合法 JSON」省一轮排查。
                raise RuntimeError("上游返回网页(HTML)而不是模型列表，地址或路径可能不对")

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
        """给上游发一个最小请求测延迟。

        不是「能连上」就叫成功：429/限流这类是**临时**状态 ——
        Key 是好的、地址也对，只是这会儿不让调。这种情况单独标成
        rate_limited，界面上和「真挂了」区分开，免得把限流误当成配置错误。
        """
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
            ctype = resp.getheader("Content-Type") or ""
            ok = resp.status < 400
            error = None
            rate_limited = False
            if ok and looks_like_html_page(ctype, body):
                # 200 但回来的是网页：地址/路径其实不对，只是网关拿首页兜底了。
                # 只按状态码判会被骗成「通，1 秒」—— 实测正是这种情况。
                ok = False
                error = "上游返回了网页（HTML）而不是 API 响应，地址或路径可能不对"
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
                # 429、或错误体里点了配额/限流的名，都算「暂时调不了」
                blob = f"{error} {body[:400].decode('utf-8', 'replace')}".lower()
                rate_limited = resp.status == 429 or any(
                    k in blob for k in ("rate limit", "ratelimit", "tpm", "rpm",
                                        "rps", "quota", "exceed"))
            result = {"id": self.id, "name": self.name, "ok": ok, "status": resp.status,
                      "latency_seconds": round(latency, 3), "model": target,
                      "error": error, "rate_limited": rate_limited, "at": time.time()}
            self._remember_check(result)
            return result
        except Exception as exc:  # noqa: BLE001
            result = {"id": self.id, "name": self.name, "ok": False, "latency_seconds": None,
                      "model": target, "error": f"{type(exc).__name__}: {exc}",
                      "rate_limited": False, "at": time.time()}
            self._remember_check(result)
            return result
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

    quota 是「这个分组里允许这家上游被调用」的额度：
    不填 = 不限量；0 = 这家不用（调用时直接跳过、排序时沉到最后）。
    """

    def __init__(self, cfg: dict):
        self.upstream_id: str = str(cfg.get("upstream") or "")
        # 上游那边的真实模型名。分组名可能和它完全不同
        self.model: str = str(cfg.get("model") or "")
        self.enabled: bool = bool(cfg.get("enabled", True))

        # None / 不填 = 不限量；0 = 不用它。
        raw_quota = cfg.get("quota")
        if raw_quota is None or raw_quota == "":
            self.quota: Optional[int] = None
        else:
            try:
                q = int(raw_quota)
            except (TypeError, ValueError):
                q = None
            # 负数没意义，当成「不限量」；0 是有意义的值，必须保留
            self.quota = q if q is not None and q >= 0 else None

    def skipped(self) -> bool:
        """额度为 0 = 这家不调用。"""
        return self.quota == 0

    def state(self) -> dict:
        out = {"upstream": self.upstream_id, "model": self.model, "enabled": self.enabled}
        # 只在设过额度时才写进状态文件，保持老状态文件干净
        if self.quota is not None:
            out["quota"] = self.quota
        return out


class ModelRoute:
    """一个分组：客户端可见的名字 → 若干上游（各用自己那边的真实名字）。

    entries 是「当前生效的顺序」，也就是原始排序。
    orderings 是保存下来的多套具名排序（比如「按测速」），
    切来切去不会弄丢原始顺序 —— 这就是「支持多个排序、设置排序名字」。
    """

    def __init__(self, name: str, cfg: Optional[dict] = None):
        cfg = cfg or {}
        self.name = name
        strategy = str(cfg.get("strategy") or "priority").lower()
        self.strategy = strategy if strategy in VALID_STRATEGIES else "priority"
        self.entries: list[RouteEntry] = [RouteEntry(e) for e in (cfg.get("entries") or [])]

        # 具名排序：名字 → 成员顺序。名字「原始」留给启动时的初始顺序，
        # 用户滚动保存测速结果时用「按测速」这类名字。
        raw_orderings = cfg.get("orderings") or {}
        self.orderings: dict[str, list[tuple[str, str]]] = {}
        if isinstance(raw_orderings, dict):
            for label, members in raw_orderings.items():
                key = str(label or "").strip()
                if not key or not isinstance(members, list):
                    continue
                parsed: list[tuple[str, str]] = []
                for m in members:
                    if isinstance(m, dict):
                        parsed.append((str(m.get("upstream") or ""),
                                       str(m.get("model") or "")))
                    elif isinstance(m, (list, tuple)) and len(m) == 2:
                        parsed.append((str(m[0]), str(m[1])))
                self.orderings[key] = parsed

    # ---- 排序 ----
    def entry_keys(self) -> list[tuple[str, str]]:
        return [(e.upstream_id, e.model) for e in self.entries]

    def apply_order(self, keys: list[tuple[str, str]]) -> bool:
        """按给定的 (上游, 真实模型名) 顺序重排 entries。

        只认当前确实存在的成员；顺序里没提到的（比如新勾选进来的）留在末尾，
        多出来的（已经不参与的了）忽略。返回是否真的变了。
        """
        by_key = {(e.upstream_id, e.model): e for e in self.entries}
        new_entries: list[RouteEntry] = []
        used: set[tuple[str, str]] = set()
        for k in keys:
            if k in by_key and k not in used:
                used.add(k)
                new_entries.append(by_key[k])
        if not new_entries:
            return False
        new_entries += [e for e in self.entries
                        if (e.upstream_id, e.model) not in used]
        if [e.state() for e in new_entries] == [e.state() for e in self.entries]:
            return False
        self.entries = new_entries
        return True

    def save_ordering(self, label: str, keys: Optional[list[tuple[str, str]]] = None) -> None:
        """把当前顺序（或给定的顺序）存成一套具名排序。"""
        name = str(label or "").strip()
        if not name:
            return
        self.orderings[name] = list(keys) if keys is not None else self.entry_keys()

    def drop_ordering(self, label: str) -> bool:
        return self.orderings.pop(str(label or "").strip(), None) is not None

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
        out = {
            "strategy": self.strategy,
            "entries": [e.state() for e in self.entries],
        }
        # 没有具名排序就不写这个字段，老状态文件不受影响
        if self.orderings:
            out["orderings"] = {
                label: [{"upstream": u, "model": m} for u, m in keys]
                for label, keys in self.orderings.items()
            }
        return out


# ---------------------------------------------------------------------------
# 请求统计的持久化（跨重启）
# ---------------------------------------------------------------------------

USAGE_KEYS = ("input", "output", "total", "cache_read", "cache_write")


class StatsStore:
    """把请求统计落到磁盘，重启后还能看。

    两个文件，都在 .llmrouter/ 下、跟着配置文件名走：
    - <stem>.stats.json   聚合结果（总量 + 按天），每来一个请求就原子写一次
    - <stem>.history.jsonl 请求流水（追加写，含输入输出）

    流水是**只增不减**的：没有删除入口，也不因为「明细上限」把已有行裁掉。
    内存里只保留最近 max_entries 条用于界面展示（_recent），文件则一直往后追加。
    当前文件攒到 rotate_at 行就改名成 <stem>.history.<时间戳>.jsonl 归档，
    然后另起一份接着写；归档文件不删，所以任何时候都能把历史翻回来。

    聚合口径和 call_stats() 一致（请求层面 + 尝试层面 + token 用量），
    它不会随重启清零，也不会因为内存明细轮换而缩水。
    """

    MAX_ENTRIES = 500        # 内存里保留的明细条数（只影响展示，不影响文件）
    JOURNAL_ROTATE_AT = 10000  # 当前流水攒到这么多行就轮转归档（约几十 MB 一份）

    def __init__(self, dir_path: str, stem: str,
                 max_entries: Optional[int] = None, rotate_at: Optional[int] = None):
        self.path = os.path.join(dir_path, f"{stem}.stats.json")
        self.history_path = os.path.join(dir_path, f"{stem}.history.jsonl")
        # 明细上限可配（[router] history_limit），只影响内存展示，不影响流水落盘
        try:
            self.max_entries = max(1, int(max_entries or self.MAX_ENTRIES))
        except (TypeError, ValueError):
            self.max_entries = self.MAX_ENTRIES
        try:
            self.rotate_at = max(self.max_entries + 1,
                                 int(rotate_at or self.JOURNAL_ROTATE_AT))
        except (TypeError, ValueError):
            self.rotate_at = self.JOURNAL_ROTATE_AT
        self._lock = threading.Lock()
        self.totals = self._fresh()
        self.days: dict[str, dict] = {}
        self._recent: list[dict] = []
        self._journal_lines = 0
        self._loaded = False

    @classmethod
    def _fresh(cls) -> dict:
        return {"calls": 0, "ok": 0, "failed": 0, "attempts": 0, "elapsed": 0.0,
                "input": 0, "output": 0, "total": 0, "cache_read": 0, "cache_write": 0,
                "upstreams": {}, "models": {}}

    @classmethod
    def _fresh_sub(cls) -> dict:
        return {"calls": 0, "ok": 0, "failed": 0, "attempts": 0, "elapsed": 0.0,
                "input": 0, "output": 0, "total": 0, "cache_read": 0, "cache_write": 0}

    @classmethod
    def _normalize(cls, raw: dict) -> dict:
        """把磁盘上的桶补齐成当前结构，老文件缺字段也能读。"""
        bucket = cls._fresh()
        for key, value in raw.items():
            if key in ("upstreams", "models") and isinstance(value, dict):
                for name, sub in value.items():
                    if isinstance(sub, dict):
                        bucket[key][str(name)] = cls._normalize(sub)
            elif key == "elapsed" and isinstance(value, (int, float)):
                bucket["elapsed"] = float(value)
            elif key in bucket and isinstance(value, (int, float)):
                bucket[key] = int(value)
        return bucket

    def _apply(self, bucket: dict, entry: dict) -> None:
        ok = bool(entry.get("ok"))
        elapsed = float(entry.get("elapsed_seconds") or 0)
        bucket["calls"] += 1
        bucket["ok" if ok else "failed"] += 1
        bucket["elapsed"] += elapsed

        trail = entry.get("trail")
        if not isinstance(trail, list) or not trail:
            trail = [{"upstream": entry.get("upstream") or "-",
                      "upstream_id": entry.get("upstream_id"),
                      "ok": ok,
                      "model": entry.get("effective_model"),
                      "status": entry.get("status")}]

        model_name = entry.get("model") or "-"
        model = bucket["models"].setdefault(model_name, self._fresh())
        model["calls"] += 1
        model["ok" if ok else "failed"] += 1
        model["elapsed"] += elapsed

        usage_counted = False
        for step in trail:
            if not isinstance(step, dict):
                continue
            up_name = step.get("upstream") or "-"
            for owner in (bucket, model):
                u = owner["upstreams"].setdefault(up_name, self._fresh_sub())
                u["calls"] += 1
                u["ok" if step.get("ok") else "failed"] += 1
            bucket["attempts"] += 1
            model["attempts"] += 1
            # token 用量只算一次（算在实际处理成功的那一步），不然会重复计数
            if not usage_counted and step.get("ok"):
                usage_counted = True
                usage = entry.get("usage") or {}
                self._add_usage(bucket, usage)
                self._add_usage(model, usage)
                self._add_usage(bucket["upstreams"][up_name], usage)
                self._add_usage(model["upstreams"][up_name], usage)

    @staticmethod
    def _add_usage(bucket: dict, usage: dict) -> None:
        for key in USAGE_KEYS:
            value = usage.get(key)
            if isinstance(value, (int, float)):
                bucket[key] += int(value)

    @classmethod
    def _merge(cls, dst: dict, src: dict) -> None:
        for key in ("calls", "ok", "failed", "attempts", "input", "output",
                    "total", "cache_read", "cache_write"):
            value = src.get(key)
            if isinstance(value, (int, float)):
                dst[key] += int(value)
        elapsed = src.get("elapsed")
        if isinstance(elapsed, (int, float)):
            dst["elapsed"] += float(elapsed)
        for nest in ("upstreams", "models"):
            for name, sub in (src.get(nest) or {}).items():
                if isinstance(sub, dict):
                    cls._merge(dst[nest].setdefault(str(name), cls._fresh()), sub)

    def record(self, entry: dict) -> None:
        at = entry.get("at")
        date = time.strftime("%Y-%m-%d", time.localtime(float(at) if at else time.time()))
        with self._lock:
            self._apply(self.totals, entry)
            self._apply(self.days.setdefault(date, self._fresh()), entry)
            self._append(entry)
            self._save()

    def _append(self, entry: dict) -> None:
        try:
            os.makedirs(os.path.dirname(self.history_path), exist_ok=True)
            with open(self.history_path, "a", encoding="utf-8") as fh:
                fh.write(json.dumps(entry, ensure_ascii=False, separators=(",", ":")) + "\n")
            self._journal_lines += 1
            if self._journal_lines >= self.rotate_at:
                self._rotate_journal()
        except Exception as exc:  # noqa: BLE001
            sys.stderr.write(f"[warn] 写历史流水失败: {exc}\n")

    def _rotate_journal(self, force: bool = False) -> Optional[str]:
        """把当前流水改名归档，另起一份新的接着写。

        只改名、不删行 —— 这是「流水不可清除」的关键：以前是重写成
        「最近 N 条」，等于把老记录真删了。改名后归档文件永久保留。

        force=True 时忽略行数阈值（手动归档用）。
        """
        if not os.path.exists(self.history_path):
            return None
        if not force and self._journal_lines < self.rotate_at:
            return None
        try:
            stamp = time.strftime("%Y%m%d-%H%M%S")
            directory = os.path.dirname(self.history_path)
            stem = os.path.splitext(os.path.basename(self.history_path))[0]
            archived = os.path.join(directory, f"{stem}.{stamp}.jsonl")
            seq = 1
            while os.path.exists(archived):
                archived = os.path.join(directory, f"{stem}.{stamp}-{seq}.jsonl")
                seq += 1
            os.replace(self.history_path, archived)
            self._journal_lines = 0
            return archived
        except Exception as exc:  # noqa: BLE001
            sys.stderr.write(f"[warn] 归档历史流水失败: {exc}\n")
            return None

    def journal_files(self) -> list[str]:
        """当前流水 + 全部归档，按文件名（即时间）排序。"""
        directory = os.path.dirname(self.history_path)
        stem = os.path.splitext(os.path.basename(self.history_path))[0]
        out: list[str] = []
        try:
            for name in sorted(os.listdir(directory)):
                if not name.endswith(".jsonl"):
                    continue
                if name == os.path.basename(self.history_path) or name.startswith(stem + "."):
                    out.append(os.path.join(directory, name))
        except OSError:
            pass
        return out

    def rotate_journal_now(self) -> Optional[str]:
        """手动归档当前流水（界面/接口用），不管有没有到自动轮转阈值。"""
        with self._lock:
            return self._rotate_journal(force=True)

    def journal_info(self) -> dict:
        """流水文件概况：当前行数、归档份数、合计大小。"""
        files = self.journal_files()
        total = 0
        for p in files:
            try:
                total += os.path.getsize(p)
            except OSError:
                pass
        return {
            "current_lines": self._journal_lines,
            "files": len(files),
            "bytes": total,
            "rotate_at": self.rotate_at,
        }

    def _save(self) -> None:
        try:
            os.makedirs(os.path.dirname(self.path), exist_ok=True)
            tmp = self.path + ".tmp"
            with open(tmp, "w", encoding="utf-8") as fh:
                json.dump({"version": 1, "totals": self.totals, "days": self.days},
                          fh, ensure_ascii=False, separators=(",", ":"))
            os.replace(tmp, self.path)
        except Exception as exc:  # noqa: BLE001
            sys.stderr.write(f"[warn] 保存统计失败: {exc}\n")

    def load(self) -> None:
        self._load_journal()
        try:
            with open(self.path, "r", encoding="utf-8") as fh:
                data = json.load(fh)
        except FileNotFoundError:
            data = None
        except Exception as exc:  # noqa: BLE001
            sys.stderr.write(f"[warn] 统计文件读不了，稍后按流水重建: {exc}\n")
            data = None
        if isinstance(data, dict):
            totals = data.get("totals")
            days = data.get("days")
            if isinstance(totals, dict) and isinstance(days, dict):
                self.totals = self._normalize(totals)
                for date, bucket in days.items():
                    if isinstance(bucket, dict):
                        self.days[str(date)] = self._normalize(bucket)
                self._loaded = bool(self.totals.get("calls"))
        # 聚合值可能比流水「慢一步」：记账是写响应之后做的，
        # 进程在两者之间被杀就会漏掉最后几条。流水是权威记录，
        # 所以启动时对一次账，落后就按流水补上（只增不减）。
        self.reconcile_from_journal()

    def journal_entries(self) -> list[dict]:
        """把当前流水和全部归档里的记录按时间顺序读出来。"""
        out: list[dict] = []
        for path in self.journal_files():
            try:
                with open(path, "r", encoding="utf-8") as fh:
                    for line in fh:
                        line = line.strip()
                        if not line:
                            continue
                        try:
                            item = json.loads(line)
                        except Exception:  # noqa: BLE001
                            continue   # 断电时最后一行可能是半条，跳过
                        if isinstance(item, dict):
                            out.append(item)
            except OSError:
                continue
        return out

    def _journal_line_count(self) -> int:
        """数一遍流水总共多少行（含归档），不解析 JSON —— 启动时要快。"""
        total = 0
        for path in self.journal_files():
            try:
                with open(path, "r", encoding="utf-8") as fh:
                    for _ in fh:
                        total += 1
            except OSError:
                continue
        return total

    def reconcile_from_journal(self, max_gap: int = 0) -> bool:
        """启动时按流水校正聚合统计。

        流水是**唯一权威记录**：聚合只是它的缓存。所以只要聚合条数和
        流水里实际条数对不上，就以流水为准重算。

        早先的版本是「只在差一点点时才补，差得多就不动」，结果出了一个
        长期对不上的现象：左边按聚合显示 375、右边按内存计数显示 188，
        而流水里其实是 1219 条 —— 谁都不对。现在统一以流水为口径，
        启动时对平，界面上的数字从此只有这一个来源。
        """
        journal_lines = self._journal_line_count()
        aggregate = self.totals.get("calls", 0)
        if journal_lines == aggregate:
            return False
        if journal_lines == 0:
            return False       # 还没有流水，保留现有聚合（可能是配置里带的）

        if max_gap and (journal_lines - aggregate) > max_gap:
            sys.stderr.write(
                f"[info] 流水 {journal_lines} 条、聚合 {aggregate} 条，"
                f"相差 {journal_lines - aggregate} 条，超出阈值未自动重算\n")
            return False

        result = self.rebuild_stats_from_journal()
        sys.stderr.write(
            "[info] 启动校正：聚合 %d 条 -> 按流水 %d 条重算（%s）\n" % (
                aggregate, result.get("calls", 0),
                ", ".join(result.get("days") or [])))
        return True

    def rebuild_stats_from_journal(self) -> dict:
        """以流水为唯一依据重建全部聚合统计（总量 + 按天 + 按模型）。

        与 reconcile_from_journal 的区别：那个是保守补记（只在差一点点时动），
        这个是**彻底重算** —— 用在「聚合明显落后于流水」这种真出问题的时候，
        由运维显式触发（/admin/stats/rebuild），不自动跑。
        """
        entries = self.journal_entries()
        with self._lock:
            self.totals = self._fresh()
            self.days = {}
            for entry in entries:
                at = entry.get("at")
                date = time.strftime("%Y-%m-%d",
                                     time.localtime(float(at) if at else time.time()))
                self._apply(self.totals, entry)
                self._apply(self.days.setdefault(date, self._fresh()), entry)
            self._loaded = True
            self._save()
        return {
            "ok": True,
            "entries": len(entries),
            "calls": self.totals["calls"],
            "days": sorted(self.days.keys()),
        }

    # ---- 全量流水查询（列表和统计都以流水为准）----
    #
    # 之前列表读的是内存里那 500 条 `_recent`，所以白天/昨天的记录翻不到，
    # 而汇总数字走的是落盘聚合 —— 两个口径必然对不上。
    # 这里统一从流水读，支持按天/按月/按分组筛，并给出该筛选下的真实条数。

    @staticmethod
    def _entry_day(entry: dict) -> str:
        at = entry.get("at")
        return time.strftime("%Y-%m-%d",
                             time.localtime(float(at) if at else time.time()))

    @staticmethod
    def _entry_group(entry: dict) -> str:
        """这条记录属于哪个分组。客户端请求的 model 就是分组名。"""
        return str(entry.get("model") or "-")

    @staticmethod
    def _entry_trail(entry: dict) -> list[dict]:
        """这条记录依次试过的上游（每次尝试一步）。

        「上游」字段只记最终成功的那一家；请求中途换过家时，被跳过的那几家
        失败只留在 trail 里。老记录没有 trail，就按最终上游合成一步。
        """
        trail = entry.get("trail")
        if isinstance(trail, list) and trail:
            return [s for s in trail if isinstance(s, dict)]
        return [{"upstream": entry.get("upstream") or "-",
                 "upstream_id": entry.get("upstream_id"),
                 "ok": bool(entry.get("ok")),
                 "model": entry.get("effective_model"),
                 "status": entry.get("status"),
                 "reason": entry.get("error")}]

    def query_journal(self, day: str = "", month: str = "", group: str = "",
                      upstream: str = "", offset: int = 0, limit: int = 100,
                      with_text: bool = False) -> dict:
        """按条件翻流水。返回 {entries, total, offset, limit, days, groups, upstreams}。

        - day:   "YYYY-MM-DD"，只看这一天
        - month: "YYYY-MM"，只看这一个月
        - group: 模型分组名，只看它
        - upstream: 上游名，只看**试过**这个上游的请求（含中途失败后换家的那些）
        - offset/limit: 分页；entries 按时间**倒序**（最新在前）
        - with_text: 是否带 request_text/response_text 这类大字段
          （列表页不需要，一直传全量会很卡，展开某条时再单独取）

        同时回传 days / groups / upstreams 各维度的计数，前端用来做下拉和筛选提示。
        upstreams 按**每次尝试**统计（和右侧卡片同一口径），不是按最终上游。
        """
        entries = self.journal_entries()
        # 过滤
        keep: list[tuple[str, dict]] = []
        day_counts: dict[str, int] = {}
        group_counts: dict[str, int] = {}
        # upstream 计数在「天/月/分组」筛选之后、上游筛选之前统计：
        # 下拉里只列当前分组真正用过的上游，并带失败数，方便一眼找到坏上游。
        up_counts: dict[str, list[int]] = {}
        for e in entries:
            d = self._entry_day(e)
            g = self._entry_group(e)
            day_counts[d] = day_counts.get(d, 0) + 1
            group_counts[g] = group_counts.get(g, 0) + 1
            if day:
                if d != day:
                    continue
            if month:
                if not d.startswith(month):
                    continue
            if group:
                if g != group:
                    continue
            steps = self._entry_trail(e)
            names = set()
            for s in steps:
                su = str(s.get("upstream") or "-")
                names.add(su)
                slot = up_counts.setdefault(su, [0, 0])
                slot[0] += 1
                if not s.get("ok"):
                    slot[1] += 1
            # 按上游筛选时匹配「这次请求试过的任意一家」，而不是只看最终上游：
            # 否则换家过程中失败的上游永远筛不出来（请求后来在别家成功了）。
            if upstream:
                if upstream not in names:
                    continue
            keep.append((d, e))

        total = len(keep)
        # 最新在前
        keep.sort(key=lambda pair: float(pair[1].get("at") or 0), reverse=True)
        try:
            offset = max(0, int(offset))
        except (TypeError, ValueError):
            offset = 0
        try:
            limit = max(1, min(1000, int(limit)))
        except (TypeError, ValueError):
            limit = 100
        page = keep[offset:offset + limit]

        out_entries: list[dict] = []
        for _, e in page:
            if with_text:
                out_entries.append(e)
            else:
                # 只挑列表要用的字段，避免把几十 MB 的正文一起发出去
                out_entries.append({
                    k: e.get(k) for k in
                    ("at", "time", "upstream_id", "upstream", "model",
                     "effective_model", "status", "elapsed_seconds", "ok",
                     "stream", "attempts", "route_source", "error", "usage",
                     "trail")
                })
        return {
            "entries": out_entries,
            "total": total,
            "offset": offset,
            "limit": limit,
            "days": dict(sorted(day_counts.items(), reverse=True)),
            "groups": dict(sorted(group_counts.items(), key=lambda kv: -kv[1])),
            "upstreams": [
                {"key": k, "calls": v[0], "failed": v[1]}
                for k, v in sorted(up_counts.items(),
                                   key=lambda kv: (-kv[1][1], -kv[1][0]))
            ],
            "filter": {"day": day, "month": month, "group": group,
                       "upstream": upstream},
            "journal": self.journal_info(),
        }

    def find_journal_entry(self, at: float, upstream: str = "",
                           status: Optional[int] = None) -> Optional[dict]:
        """按时间戳（+上游/状态辅助定位）取回**含正文**的那一条记录。

        列表接口不带 request_text/response_text（几十 MB 发出去太慢），
        所以用户点开某条详情时，用这个按需取回完整内容。

        时间戳是浮点数，经 URL 传一圈会有末位精度损失，所以按 0.1 秒
        比对（和列表页的 key 规则一致）；同一 0.1 秒里有多条时，
        再用上游名/状态挑最匹配的那条。
        """
        try:
            target = float(at)
        except (TypeError, ValueError):
            return None
        # 优先精确命中：高并发时几条记录可能落在同一个 0.1 秒里，
        # 只按 0.1 秒窗匹配会取回「同一格里的另一条」，正文就对不上了。
        loose = None
        for path in reversed(self.journal_files()):
            try:
                with open(path, "r", encoding="utf-8") as fh:
                    for line in fh:
                        line = line.strip()
                        if not line:
                            continue
                        try:
                            item = json.loads(line)
                        except Exception:  # noqa: BLE001
                            continue
                        if not isinstance(item, dict) or not item.get("at"):
                            continue
                        try:
                            value = float(item["at"])
                        except (TypeError, ValueError):
                            continue
                        if upstream and str(item.get("upstream") or "") != upstream:
                            continue
                        if status is not None and item.get("status") != status:
                            continue
                        if value == target:
                            return item          # 精确命中，直接用它
                        if abs(value - target) < 0.1 and loose is None:
                            loose = item          # 兜底：同一 0.1 秒窗内
            except OSError:
                continue
        return loose

    def journal_retention(self) -> dict:
        """流水保留情况：多少天、共多少条、最早最晚。"""
        entries = self.journal_entries()
        if not entries:
            return {"entries": 0, "days": 0, "first": None, "last": None}
        ts = [float(e["at"]) for e in entries if e.get("at")]
        if not ts:
            return {"entries": len(entries), "days": 0, "first": None, "last": None}
        day_counts = {}
        for t in ts:
            d = time.strftime("%Y-%m-%d", time.localtime(t))
            day_counts[d] = day_counts.get(d, 0) + 1
        return {
            "entries": len(entries),
            "days": len(day_counts),
            "first": time.strftime("%Y-%m-%d %H:%M:%S", time.localtime(min(ts))),
            "last": time.strftime("%Y-%m-%d %H:%M:%S", time.localtime(max(ts))),
            "by_day": dict(sorted(day_counts.items(), reverse=True)),
        }

    def journal_index(self) -> dict:
        """流水里出现过的日期 / 月份 / 分组，以及各自条数。

        前端用它生成筛选下拉 —— 只列真有过请求的那几天，不给空选项。
        结果按文件（名字+大小+修改时间）缓存，避免每次开页面都全量扫。
        """
        files = self.journal_files()
        try:
            stamp = tuple((os.path.basename(p), os.path.getsize(p), os.path.getmtime(p))
                          for p in files if os.path.exists(p))
        except OSError:
            stamp = ()

        cache = getattr(self, "_journal_index_cache", None)
        if cache is not None and cache.get("stamp") == stamp:
            return cache["value"]

        days: dict[str, int] = {}
        months: dict[str, int] = {}
        groups: dict[str, int] = {}
        total = 0
        for path in files:
            try:
                with open(path, "r", encoding="utf-8") as fh:
                    for line in fh:
                        line = line.strip()
                        if not line:
                            continue
                        try:
                            item = json.loads(line)
                        except Exception:  # noqa: BLE001
                            continue
                        if not isinstance(item, dict):
                            continue
                        total += 1
                        at = item.get("at")
                        if at:
                            try:
                                st = time.localtime(float(at))
                                d = time.strftime("%Y-%m-%d", st)
                                days[d] = days.get(d, 0) + 1
                                mo = time.strftime("%Y-%m", st)
                                months[mo] = months.get(mo, 0) + 1
                            except (TypeError, ValueError, OSError):
                                pass
                        g = self._entry_group(item)
                        groups[g] = groups.get(g, 0) + 1
            except OSError:
                continue

        value = {
            "total": total,
            "days": [{"key": k, "calls": v}
                     for k, v in sorted(days.items(), reverse=True)],
            "months": [{"key": k, "calls": v}
                       for k, v in sorted(months.items(), reverse=True)],
            "groups": [{"key": k, "calls": v}
                       for k, v in sorted(groups.items(), key=lambda kv: -kv[1])],
        }
        self._journal_index_cache = {"stamp": stamp, "value": value}
        return value

    def _load_journal(self) -> None:
        try:
            with open(self.history_path, "r", encoding="utf-8") as fh:
                lines = fh.readlines()
        except FileNotFoundError:
            return
        except Exception as exc:  # noqa: BLE001
            sys.stderr.write(f"[warn] 历史明细读不了: {exc}\n")
            return
        self._journal_lines = len(lines)
        for line in lines[-self.max_entries:]:
            try:
                item = json.loads(line)
            except Exception:  # noqa: BLE001
                continue     # 断电时最后一行可能是半条，跳过
            if isinstance(item, dict):
                self._recent.append(item)
        # 当前流水太大就归档（改名保留，不删行）
        if self._journal_lines >= self.rotate_at:
            self._rotate_journal()

    def recent_entries(self) -> list[dict]:
        return list(self._recent)

    def has_data(self) -> bool:
        return self._loaded or self.totals["calls"] > 0

    def reindex(self, entries: list[dict]) -> None:
        """统计文件丢了（或被删）时，用明细重新累计一遍。"""
        with self._lock:
            for entry in entries:
                at = entry.get("at")
                date = time.strftime("%Y-%m-%d",
                                     time.localtime(float(at) if at else time.time()))
                self._apply(self.totals, entry)
                self._apply(self.days.setdefault(date, self._fresh()), entry)
            self._loaded = True
            if entries:
                self._save()

    def clear(self) -> None:
        """已停用：流水和统计都不允许被清除。

        以前这个方法会删掉流水文件和聚合文件，界面上一个「清空统计」按钮
        就能把它们全抹掉 —— 误点一下就没有了。现在统计只会往上加。
        留着这个函数（而不是删掉）是为了让误调用报错看得懂。
        """
        raise RuntimeError(
            "统计与请求流水不可清除（流水是只增不减的记录）。"
            "如需重新开始，请改用另一份配置文件。")

    def meta(self) -> dict:
        with self._lock:
            return {
                "days": sorted(self.days.keys(), reverse=True),
                "months": sorted({d[:7] for d in self.days}, reverse=True),
                "totals": self._light(self.totals),
            }

    def snap_range(self, key: str) -> dict:
        """按范围聚合，结构对齐 call_stats()，前端可以直接复用渲染。"""
        with self._lock:
            bucket = self._range_bucket(key)
            return self._render(bucket, key or "all", self.max_entries)

    def _range_bucket(self, key: str) -> dict:
        key = (key or "all").strip()
        if key.startswith("day:"):
            bucket = self.days.get(key[4:])
            return self._clone(bucket) if bucket else self._fresh()
        if key.startswith("month:"):
            prefix = key[6:]
            merged = self._fresh()
            for date, bucket in self.days.items():
                if date.startswith(prefix):
                    self._merge(merged, bucket)
            return merged
        return self._clone(self.totals)

    @classmethod
    def _clone(cls, bucket: dict) -> dict:
        return cls._normalize(bucket)

    @classmethod
    def _light(cls, bucket: dict) -> dict:
        return {k: int(bucket.get(k, 0) or 0) for k in
                ("calls", "ok", "failed", "attempts",
                 "input", "output", "total", "cache_read", "cache_write")}

    @classmethod
    def _render(cls, bucket: dict, key: str, history_limit: int = MAX_ENTRIES) -> dict:
        groups = []
        for name, m in bucket.get("models", {}).items():
            ups = sorted(
                [{"upstream": un, "calls": int(u.get("calls", 0)),
                  "ok": int(u.get("ok", 0)), "failed": int(u.get("failed", 0)),
                  "usage": cls._usage_of(u)}
                 for un, u in (m.get("upstreams") or {}).items()],
                key=lambda x: -x["calls"])
            calls = int(m.get("calls", 0))
            groups.append({
                "group": name,
                "calls": calls, "ok": int(m.get("ok", 0)),
                "failed": int(m.get("failed", 0)),
                "attempts": int(m.get("attempts", 0)),
                "avg_elapsed": round(float(m.get("elapsed", 0.0)) / calls, 3)
                               if calls else 0,
                "usage": cls._usage_of(m),
                "upstreams": ups, "upstream_count": len(ups),
            })
        groups.sort(key=lambda x: -x["calls"])
        ups = sorted(
            [{"upstream": n, "calls": int(u.get("calls", 0)), "ok": int(u.get("ok", 0)),
              "failed": int(u.get("failed", 0))}
             for n, u in bucket.get("upstreams", {}).items()],
            key=lambda x: -x["calls"])
        return {"groups": groups, "totals": cls._light(bucket),
                "upstreams": ups, "history_limit": history_limit, "range": key}

    @staticmethod
    def _usage_of(bucket: dict) -> dict:
        out = {}
        for key in USAGE_KEYS:
            value = bucket.get(key)
            if value:
                out[key] = int(value)
        return out


# ---------------------------------------------------------------------------
# 路由器
# ---------------------------------------------------------------------------

class Router:
    def __init__(self, cfg: dict, config_path: str):
        self.config_path = config_path
        self.lock = threading.Lock()
        self.rr_counters: dict[str, int] = {}
        self.started_at = time.time()
        self.total_requests = 0

        # 明细上限只影响「列表能看到多少条」，累计统计是全量的（见 StatsStore）
        router_cfg_pre = cfg.get("router") or {}
        try:
            hist_limit = int(router_cfg_pre.get("history_limit") or 0)
        except (TypeError, ValueError):
            hist_limit = 0
            sys.stderr.write("[warn] history_limit 不是数字，按默认值处理\n")
        self.history: collections.deque = collections.deque(
            maxlen=hist_limit if hist_limit > 0 else StatsStore.MAX_ENTRIES)

        # 统计要跨重启：先把上次的明细找回来，再按需重建聚合
        self.stats = StatsStore(self._state_dir(), self._state_stem(),
                                max_entries=self.history.maxlen)
        self.stats.load()
        self.history.extend(self.stats.recent_entries())
        if not self.stats.has_data() and self.history:
            self.stats.reindex(list(self.history))

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
        # 组里所有上游都「并发满」时，最多等这么久碰运气（秒）。
        # 设 0 = 不等，直接回 429。
        try:
            self.queue_timeout = max(0.0, float(router_cfg.get("queue_timeout_seconds",
                                                               QUEUE_TIMEOUT_DEFAULT)))
        except (TypeError, ValueError):
            self.queue_timeout = QUEUE_TIMEOUT_DEFAULT

        self.upstreams: list[Upstream] = []
        self.models: dict[str, ModelRoute] = {}
        # 用户显式删掉的路由：sync_routes 不许按「勾选+分组」把它再建回来，
        # 直到用户重新保存这个路由 / 手动分组 / 自动配好为止。
        self.suppressed_models: set[str] = set()

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
                # 容量上限也可以写在配置文件里（网页上能改，会覆盖这里）
                "max_concurrent": u.get("max_concurrent"),
                "max_per_minute": u.get("max_per_minute"),
            })
        models_out: dict[str, dict] = {}
        for name, mcfg in (cfg.get("models") or {}).items():
            models_out[name] = {
                "strategy": mcfg.get("strategy", "priority"),
                "entries": [
                    {"upstream": e.get("upstream"), "model": e.get("model") or name,
                     "enabled": e.get("enabled", True),
                     "quota": e.get("quota")}
                    for e in (mcfg.get("entries") or [])
                ],
                "orderings": mcfg.get("orderings") or {},
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

        raw_suppressed = data.get("suppressed_models")
        self.suppressed_models = {str(x) for x in raw_suppressed} if isinstance(raw_suppressed, list) else set()

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
    #
    # 每次保存前会把当前版本轮转进 versions/ 留成带时间戳的快照（保留最近
    # STATE_BACKUP_KEEP 份），reset 时也不是删除而是移进 trash/。
    # 早先只留单份 .bak，坑在于：最后一次写入会把上一版好配置挤掉 ——
    # 一旦配置被误删，.bak 里已经是被删之后的状态，等于没有退路。

    STATE_DIRNAME = ".llmrouter"
    VERSIONS_DIRNAME = "versions"

    def _state_dir(self) -> str:
        directory = os.path.dirname(os.path.abspath(self.config_path))
        return os.path.join(directory, self.STATE_DIRNAME)

    def _versions_dir(self) -> str:
        return os.path.join(self._state_dir(), self.VERSIONS_DIRNAME)

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

    def _snapshot_state(self, path: str) -> None:
        """把即将被覆盖的状态存一份带时间戳的快照，并只保留最近若干份。

        时间戳精确到秒，同一秒内连改两次会撞名 —— 撞上就补一个序号，
        宁可多留一份也不覆盖（快照的作用就是「还能回退」）。
        """
        versions = self._versions_dir()
        try:
            os.makedirs(versions, exist_ok=True)
            stem = f"{self._state_stem()}.state.{time.strftime('%Y%m%d-%H%M%S')}"
            target = os.path.join(versions, stem + ".json")
            seq = 1
            while os.path.exists(target):
                target = os.path.join(versions, f"{stem}-{seq}.json")
                seq += 1
            shutil.copyfile(path, target)

            # 只保留最近 N 份，按文件名排序即按时间排序
            olds = sorted(f for f in os.listdir(versions)
                          if f.startswith(f"{self._state_stem()}.state.")
                          and f.endswith(".json"))
            for name in olds[:-STATE_BACKUP_KEEP]:
                try:
                    os.remove(os.path.join(versions, name))
                except OSError:
                    pass
        except Exception as exc:  # noqa: BLE001
            sys.stderr.write(f"[warn] 状态快照失败（不影响本次保存）: {exc}\n")

    def list_state_versions(self) -> list[dict]:
        """列出可回退的状态快照，最新的在前。"""
        versions = self._versions_dir()
        if not os.path.isdir(versions):
            return []
        out: list[dict] = []
        prefix = f"{self._state_stem()}.state."
        for name in sorted(os.listdir(versions), reverse=True):
            if not (name.startswith(prefix) and name.endswith(".json")):
                continue
            full = os.path.join(versions, name)
            try:
                st = os.stat(full)
            except OSError:
                continue
            item: dict[str, Any] = {
                "file": name,
                "size": st.st_size,
                "mtime": st.st_mtime,
            }
            try:
                with open(full, "r", encoding="utf-8") as fh:
                    data = json.load(fh)
                if isinstance(data, dict):
                    models = data.get("models")
                    item["upstreams"] = len(data.get("upstreams") or [])
                    item["models"] = len(models) if isinstance(models, dict) else 0
            except Exception:  # noqa: BLE001
                pass
            out.append(item)
        return out

    def restore_state_version(self, name: str) -> dict:
        """按快照文件名回退状态（回退前先把当前状态也存一份，可再退回来）。"""
        # 只认文件名，挡掉 ../ 这种越界路径
        base = os.path.basename(str(name or ""))
        if not base or base != str(name) or not base.endswith(".json"):
            raise ValueError("快照文件名不合法")
        versions = self._versions_dir()
        src = os.path.join(versions, base)
        if not os.path.isfile(src):
            raise KeyError(f"找不到快照 {base!r}")
        try:
            with open(src, "r", encoding="utf-8") as fh:
                data = json.load(fh)
        except Exception as exc:  # noqa: BLE001
            raise ValueError(f"快照读不了: {exc}") from exc
        if not isinstance(data, dict) or not isinstance(data.get("upstreams"), list):
            raise ValueError("快照内容不像是状态文件")

        # 先把「现在」存一份，这样回退本身也是可撤销的
        current = self._state_path()
        if os.path.exists(current):
            self._snapshot_state(current)

        self._apply_state(data)
        self._save_state()
        return {
            "ok": True,
            "restored_from": base,
            "upstreams": len(self.upstreams),
            "models": len(self.models),
        }

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
            "suppressed_models": sorted(self.suppressed_models),
        }
        path = self._state_path()
        try:
            os.makedirs(self._state_dir(), exist_ok=True)

            # 覆盖前轮转出一份带时间戳的快照（versions/ 下保留最近若干份）。
            # 同时照旧留一份同名 .bak —— 老用户和 README 里都认这个名字。
            if os.path.exists(path):
                self._snapshot_state(path)
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

    # ---- 管理操作审计 ----
    #
    # 每个改配置的管理接口都往 .llmrouter/admin.log 追加一行（JSON Lines）。
    # 起因：配置在没人操作的时间点被改过两次，却查不出是谁干的 ——
    # 上游增删、路由改动这类操作必须留下痕迹。

    def _admin_log_path(self) -> str:
        return os.path.join(self._state_dir(),
                            f"{self._state_stem()}{ADMIN_LOG_SUFFIX}")

    def admin_log(self, op: str, detail: Optional[dict] = None,
                  actor: str = "", ok: bool = True) -> None:
        entry = {
            "at": time.time(),
            "time": time.strftime("%Y-%m-%d %H:%M:%S"),
            "op": op,
            "ok": bool(ok),
            "actor": actor or "-",
            "detail": detail or {},
        }
        try:
            path = self._admin_log_path()
            os.makedirs(os.path.dirname(path), exist_ok=True)
            with self.lock:
                with open(path, "a", encoding="utf-8") as fh:
                    fh.write(json.dumps(entry, ensure_ascii=False,
                                        separators=(",", ":")) + "\n")
        except Exception as exc:  # noqa: BLE001
            sys.stderr.write(f"[warn] 写审计日志失败: {exc}\n")

    def read_admin_log(self, limit: int = 200) -> list[dict]:
        """读最近的管理操作记录，最新的在前。"""
        path = self._admin_log_path()
        try:
            with open(path, "r", encoding="utf-8") as fh:
                lines = fh.readlines()
        except FileNotFoundError:
            return []
        except Exception as exc:  # noqa: BLE001
            sys.stderr.write(f"[warn] 审计日志读不了: {exc}\n")
            return []
        out: list[dict] = []
        for line in reversed(lines[-max(1, int(limit)):]):
            try:
                item = json.loads(line)
            except Exception:  # noqa: BLE001
                continue
            if isinstance(item, dict):
                out.append(item)
        return out

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
            "max_concurrent": payload.get("max_concurrent"),
            "max_per_minute": payload.get("max_per_minute"),
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
        # 容量上限：显式传 null/"" 表示清掉（回到不限量）
        if "max_concurrent" in payload:
            up.max_concurrent = self._pos_int(payload.get("max_concurrent"))
        if "max_per_minute" in payload:
            up.max_per_minute = self._pos_int(payload.get("max_per_minute"))
        self._save_state()
        return up

    def set_capacity(self, uid: str, max_concurrent: Any = None,
                     max_per_minute: Any = None) -> dict:
        """设置某个上游的并发/速率上限（0 或空 = 不限量）。

        上限挂在**上游**身上而不是某个分组：它反映的是这个账号/Key 的
        真实配额，同一个账号被好几个分组引用时，配额是共用的一份。
        按分组各记一套的话，两个分组各给 5 并发，实际就会同时打进 10 个，
        等于上限根本没生效。
        """
        up = self.find_upstream(uid)
        if up is None:
            raise KeyError(f"找不到上游 {uid!r}")
        up.max_concurrent = self._pos_int(max_concurrent)
        up.max_per_minute = self._pos_int(max_per_minute)
        self._save_state()
        return {"ok": True, "id": up.id, "name": up.name,
                "max_concurrent": up.max_concurrent,
                "max_per_minute": up.max_per_minute,
                "capacity": up.capacity_state()}

    @staticmethod
    def _pos_int(value: Any) -> Optional[int]:
        """把传入的上限读成正整数；空/0/负数/看不懂的一律当「不限」。"""
        return Upstream._pos_int(value)

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
                group = up.group_of(real)
                # 用户显式删过这个名称的路由，就别再自动建回来
                if group in self.suppressed_models:
                    continue
                target.setdefault(group, []).append((up.id, real))

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
            g = str(group or "").strip()
            if g:
                # 用户主动把模型归到这个分组名下 → 说明要这个路由，撤销删除抑制
                self.suppressed_models.discard(g)
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

        # 删除抑制跟着一起改名；改名本身是用户主动操作，新名字不再抑制
        if old in self.suppressed_models:
            self.suppressed_models.discard(old)
            self.suppressed_models.add(new)

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
                "quota": e.get("quota"),
            }))
        if not entries:
            raise ValueError("至少要给这个分组选一个上游模型")

        # 保存路由时保留原来的具名排序（比如「原始」「按测速」）。
        # 这里一个都不能动 —— 尤其「原始」记的是用户最初的顺序，
        # 要是跟着当前顺序走，切回去就回不到真正的原样了。
        old = self.models.get(name)
        orderings = dict(old.orderings) if old is not None else {}

        route = ModelRoute(name, {"strategy": strategy,
                                  "entries": [e.state() for e in entries],
                                  "orderings": {
                                      label: [{"upstream": u, "model": m} for u, m in keys]
                                      for label, keys in orderings.items()
                                  }})
        self.models[name] = route
        # 手动保存 = 明确要这个路由，撤销之前的删除抑制
        self.suppressed_models.discard(name)
        self._save_state()
        return route

    # ---- 排序：多套具名排序（保存 / 切换 / 按测速重排）----
    ORDERING_ORIGINAL = "原始"

    @classmethod
    def _check_label(cls, label: str) -> None:
        """「原始」是自动留底的名字，不让用户拿去命名，免得把回溯点覆盖掉。"""
        if label == cls.ORDERING_ORIGINAL:
            raise ValueError(f"「{cls.ORDERING_ORIGINAL}」是自动留底用的名字，"
                             f"换个名字吧（它已经存在，直接切过去就行）")

    def _route_or_raise(self, name: str) -> ModelRoute:
        route = self.models.get(str(name or ""))
        if route is None:
            raise KeyError(f"没有配置模型 {name!r}")
        return route

    def list_orderings(self, name: str) -> dict:
        """列出某个分组的所有具名排序。"""
        route = self._route_or_raise(name)
        return {
            "ok": True,
            "name": route.name,
            "active": (self._active_ordering(route) or ""),
            "orderings": [
                {"label": label,
                 "members": [{"upstream": u, "model": m} for u, m in keys]}
                for label, keys in route.orderings.items()
            ],
            "entries": [{"upstream": e.upstream_id, "model": e.model,
                         "enabled": e.enabled, "quota": e.quota}
                        for e in route.entries],
        }

    @staticmethod
    def _active_ordering(route: ModelRoute) -> Optional[str]:
        """当前 entries 顺序正好等于哪一套具名排序（没有就返回 None）。"""
        keys = route.entry_keys()
        for label, saved in route.orderings.items():
            if saved == keys:
                return label
        return None

    def save_ordering(self, name: str, label: str,
                      order: Optional[list] = None) -> dict:
        """把一套顺序存成具名排序。

        - 不传 order：存「当前顺序」。
        - 传 order（[{"upstream":..., "model":...}]）：先应用这套顺序再存。

        无论哪种，第一次存排序时都会先把此刻的顺序留成一套「原始」，
        这样以后随时能切回去。
        """
        route = self._route_or_raise(name)
        label = str(label or "").strip()
        if not label:
            raise ValueError("排序名字不能为空")
        self._check_label(label)

        if self.ORDERING_ORIGINAL not in route.orderings:
            # 先留底，再动顺序 —— 顺序反了「原始」就记成新顺序了
            route.orderings[self.ORDERING_ORIGINAL] = route.entry_keys()
        keys = self._parse_order_keys(order) if isinstance(order, list) else None
        if keys:
            route.apply_order(keys)
        route.save_ordering(label)
        self._save_state()
        return {"ok": True, "name": route.name, "label": label,
                "active": self._active_ordering(route) or "",
                "entries": [{"upstream": e.upstream_id, "model": e.model,
                             "enabled": e.enabled, "quota": e.quota}
                            for e in route.entries]}

    @staticmethod
    def _parse_order_keys(order: list) -> Optional[list[tuple[str, str]]]:
        """把 [{"upstream":..,"model":..}] 解析成 [(上游, 真实模型名)]。"""
        keys: list[tuple[str, str]] = []
        for o in order:
            if not isinstance(o, dict):
                continue
            uid = str(o.get("upstream") or "")
            mid = str(o.get("model") or "")
            if uid and mid:
                keys.append((uid, mid))
        return keys or None

    def apply_ordering(self, name: str, label: str) -> dict:
        """切换到某一套已经存过的排序。"""
        route = self._route_or_raise(name)
        label = str(label or "").strip()
        if label and label not in route.orderings:
            raise KeyError(f"没有名为 {label!r} 的排序")
        if label:
            route.apply_order(route.orderings[label])
            self._save_state()
        return {"ok": True, "name": route.name, "label": label,
                "active": self._active_ordering(route) or "",
                "entries": [{"upstream": e.upstream_id, "model": e.model,
                             "enabled": e.enabled, "quota": e.quota}
                            for e in route.entries]}

    def delete_ordering(self, name: str, label: str) -> dict:
        """删掉一套具名排序。当前顺序不变，只是这个名字没了。"""
        route = self._route_or_raise(name)
        label = str(label or "").strip()
        if label == self.ORDERING_ORIGINAL:
            raise ValueError("「原始」排序是回溯用的，不能删")
        if not route.drop_ordering(label):
            raise KeyError(f"没有名为 {label!r} 的排序")
        self._save_state()
        return {"ok": True, "name": route.name, "label": label,
                "active": self._active_ordering(route) or ""}

    def reorder_by_speed(self, name: str, label: str = "按测速",
                         results: Optional[list[dict]] = None) -> dict:
        """按测速结果给某一个分组重排，并存成一套具名排序（原顺序不丢）。

        results 里每一项形如 {"upstream": "u1", "latency_seconds": 0.4}，
        没测到、测失败的排到最后。额度为 0（不调用）的也一律沉到最后——
        它本来就不参与调用，不该占别人的位置。
        """
        route = self._route_or_raise(name)
        label = str(label or "按测速").strip() or "按测速"
        if not isinstance(results, list) or not results:
            raise ValueError("缺少测速结果，先点「测速」再重排")
        self._check_label(label)

        # 优先按 (上游, 模型) 精确匹配延迟；没有 model 的结果退回按上游匹配。
        lat: dict[tuple[str, str], Optional[float]] = {}
        lat_uid: dict[str, Optional[float]] = {}
        for r in results:
            if not isinstance(r, dict):
                continue
            uid = str(r.get("upstream") or r.get("id") or "")
            if not uid:
                continue
            v = r.get("latency_seconds")
            ok = r.get("ok", v is not None)
            val = float(v) if (ok and isinstance(v, (int, float))) else None
            m = r.get("model")
            if m:
                lat[(uid, str(m))] = val
            else:
                lat_uid[uid] = val

        def sort_key(idx_entry: tuple[int, RouteEntry]):
            idx, e = idx_entry
            if e.skipped():
                return (2, 0.0, idx)          # 额度 0：直接放最后
            v = lat.get((e.upstream_id, e.model))
            if v is None and (e.upstream_id, e.model) not in lat:
                v = lat_uid.get(e.upstream_id)
            # 没结果的、失败的：排到（非 0 额度里的）最后，内部保持原相对顺序
            return (0 if v is not None else 1, v if v is not None else 0.0, idx)

        ordered = [e for _, e in sorted(enumerate(route.entries), key=sort_key)]
        keys = [(e.upstream_id, e.model) for e in ordered]

        if self.ORDERING_ORIGINAL not in route.orderings:
            route.orderings[self.ORDERING_ORIGINAL] = route.entry_keys()
        route.apply_order(keys)
        route.save_ordering(label, keys)
        self._save_state()
        return {"ok": True, "name": route.name, "label": label,
                "active": self._active_ordering(route) or "",
                "order": [{"upstream": u, "model": m} for u, m in keys],
                "entries": [{"upstream": e.upstream_id, "model": e.model,
                             "enabled": e.enabled, "quota": e.quota}
                            for e in route.entries]}

    def set_quota(self, name: str, items: list[dict]) -> dict:
        """设置某个分组里各上游的调用额度。

        items 里每一项形如 {"upstream": "u1", "model": "ds1", "quota": 0}。
        model 省略时匹配这个上游在该分组里的任意成员。
        quota 为 null / 空 = 不限量；0 = 不调用它（调用时跳过、排序时沉到最后）。
        """
        route = self._route_or_raise(name)
        if not isinstance(items, list) or not items:
            raise ValueError("没有要设置的额度")

        applied: list[dict] = []
        for it in items:
            if not isinstance(it, dict):
                continue
            uid = str(it.get("upstream") or "")
            mid = it.get("model")
            mid = str(mid) if mid not in (None, "") else None
            raw = it.get("quota")
            if raw == "":
                raw = None
            try:
                quota = None if raw is None else int(raw)
            except (TypeError, ValueError):
                raise ValueError(f"额度必须是整数或留空: {raw!r}")
            # 负数没意义，当「不限量」
            if quota is not None and quota < 0:
                quota = None

            hit = None
            for e in route.entries:
                if e.upstream_id != uid:
                    continue
                if mid is not None and e.model != mid:
                    continue
                hit = e
                break
            if hit is None:
                continue            # 这个上游不在该分组里，跳过
            hit.quota = quota
            applied.append({"upstream": hit.upstream_id, "model": hit.model,
                            "quota": quota, "skipped": quota == 0})

        if not applied:
            raise KeyError("这个分组里没有匹配的上游")
        self._save_state()
        return {"ok": True, "name": route.name, "applied": applied,
                "entries": [{"upstream": e.upstream_id, "model": e.model,
                             "enabled": e.enabled, "quota": e.quota}
                            for e in route.entries]}

    def delete_model(self, name: str) -> dict:
        if name not in self.models:
            raise KeyError(f"没有配置模型 {name!r}")
        del self.models[name]
        # 记一笔：这是用户明确删的。sync_routes 是按「勾选+分组」自动重建路由的，
        # 不记住删除，下一次任何勾选变化都会把它建回来（删了又冒出来）。
        self.suppressed_models.add(name)
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
            # 「自动配好」是用户点名要配的，撤销此前的删除抑制
            self.suppressed_models.discard(group)
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
                # 额度设成 0 = 这家不用，调用时直接跳过
                if e.skipped():
                    continue
                up = self.find_upstream(e.upstream_id)
                if up is None or not up.enabled:
                    continue
                pairs.append((up, e.model or model))
            source = "配置"
            if pairs:
                return self._order(pairs, route.strategy, model), source
            # 配了但全被停用/删光/额度为 0，退回自动
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

        分两个层面统计，和界面上的两处数字严格对应：

        - 请求层面（calls/ok/failed）：一次客户端请求算一次，
          只看最终结果。和左上角「总请求 / 成功 / 失败」一致。
        - 尝试层面（attempts，以及每个上游的 calls/ok/failed）：
          请求发给上游算一次，失败换下一个也算。**和右上角上游卡片
          的「请求 / 成功 / 失败」是同一口径**，这样一处失败不会只出现在一边。

        数据来源是 history（内存里最近 maxlen 条明细），只反映最近这些请求。
        它只用来给「刚发生、还没落进聚合」的几条补显示；
        界面上的统计数字一律走 stats_range（今天/本月/全部），
        那是全量累计、跨重启保留的，明细被裁剪不会让它缩水。
        """
        with self.lock:
            hist = list(self.history)

        groups: dict[str, dict] = {}
        for h in hist:
            name = h.get("model") or "-"
            g = groups.setdefault(name, {
                "group": name,
                "calls": 0, "ok": 0, "failed": 0,      # 请求层面
                "attempts": 0,                          # 尝试层面
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

            # 这次请求依次试过的上游。老记录没有 trail 字段，
            # 就退回用「最后落到的那家 + 本次结果」补一条，保证旧数据也能统计。
            trail = h.get("trail")
            if not trail:
                trail = [{"upstream": h.get("upstream") or "-",
                          "upstream_id": h.get("upstream_id"),
                          "ok": bool(h.get("ok")),
                          "model": h.get("effective_model"),
                          "status": h.get("status")}]

            usage_counted = False
            for step in trail:
                if not isinstance(step, dict):
                    continue
                up_name = step.get("upstream") or "-"
                u = g["upstreams"].setdefault(up_name, {
                    "upstream": up_name,
                    "upstream_id": step.get("upstream_id"),
                    "calls": 0, "ok": 0, "failed": 0, "usage": {},
                })
                u["calls"] += 1
                g["attempts"] += 1
                if step.get("ok"):
                    u["ok"] += 1
                else:
                    u["failed"] += 1
                # token 用量只算一次（算在实际处理成功的那一步上），
                # 不然一次请求试了三家会把用量记三遍
                if not usage_counted and step.get("ok"):
                    usage_counted = True
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
                "attempts": g["attempts"],
                "avg_elapsed": round(g["total_elapsed"] / g["calls"], 3) if g["calls"] else 0,
                "usage": g["usage"],
                "upstreams": ups,
                "upstream_count": len(ups),
            })
        out.sort(key=lambda x: -x["calls"])

        # 总量（请求层面 + 尝试层面，两个口径都给）
        totals = {"calls": len(hist), "ok": 0, "failed": 0, "attempts": 0,
                  "input": 0, "output": 0, "total": 0,
                  "cache_read": 0, "cache_write": 0}
        for h in hist:
            if h.get("ok"):
                totals["ok"] += 1
            else:
                totals["failed"] += 1
            trail = h.get("trail") or []
            totals["attempts"] += len(trail) if trail else 1
            use = h.get("usage") or {}
            for key in ("input", "output", "total", "cache_read", "cache_write"):
                if use.get(key) is not None:
                    totals[key] += int(use[key])

        # 各上游的尝试汇总（跨分组），和右上角卡片同口径，方便一眼对照
        per_upstream: dict[str, dict] = {}
        for g in out:
            for u in g["upstreams"]:
                agg = per_upstream.setdefault(u["upstream"], {
                    "upstream": u["upstream"], "calls": 0, "ok": 0, "failed": 0})
                agg["calls"] += u["calls"]
                agg["ok"] += u["ok"]
                agg["failed"] += u["failed"]
        upstreams = sorted(per_upstream.values(), key=lambda x: -x["calls"])

        return {"groups": out, "totals": totals, "upstreams": upstreams,
                "history_limit": self.history.maxlen,
                "history_count": len(hist)}

    # ---- 快照 ----
    def snapshot(self) -> dict:
        # 上游卡片上的「请求/成功/失败」改用**落盘聚合**的数字（跨重启不归零）。
        # 以前用 Upstream.requests 这种内存计数，重启就回零，于是
        # 左侧说 375、右侧说 188 —— 两个口径必然打架。
        per_up = {}
        for name, sub in (self.stats.totals.get("upstreams") or {}).items():
            if isinstance(sub, dict):
                per_up[name] = {
                    "requests": sub.get("calls", 0),
                    "successes": sub.get("ok", 0),
                    "failures": sub.get("failed", 0),
                }

        ups_out = []
        for u in self.upstreams:
            snap = u.snapshot()
            persistent = per_up.get(u.name)
            if persistent is not None:
                snap["requests"] = persistent["requests"]
                snap["successes"] = persistent["successes"]
                snap["failures"] = persistent["failures"]
                snap["counters_persistent"] = True
            else:
                snap["counters_persistent"] = False
            ups_out.append(snap)

        return {
            "version": __version__,
            "config_path": os.path.abspath(self.config_path),
            "state_path": os.path.abspath(self._state_path()),
            "journal": self.stats.journal_info(),
            "uptime_seconds": round(time.time() - self.started_at, 1),
            "total_requests": self.total_requests,
            "failure_threshold": self.failure_threshold,
            "cooldown_seconds": self.cooldown_default,
            "default_strategy": self.default_strategy,
            "suppressed_models": sorted(self.suppressed_models),
            "upstreams": ups_out,
            "models": {
                name: {
                    **route.state(),
                    # 每个成员：上游名 + 那边用的真实模型名 + 额度
                    "members": [
                        {
                            "upstream": e.upstream_id,
                            "upstream_name": (up.name if (up := self.find_upstream(e.upstream_id))
                                              else e.upstream_id),
                            "model": e.model,
                            "renamed": e.model != name,
                            "enabled": e.enabled,
                            "quota": e.quota,
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
                        if e.enabled and not e.skipped()
                        and (u := self.find_upstream(e.upstream_id)) and u.enabled
                    ]),
                    # 当前顺序对应哪套具名排序
                    "active_ordering": self._active_ordering(route),
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
# Anthropic Messages API 适配（/v1/messages）
#
# 上游绝大多数只认 OpenAI 的 /v1/chat/completions，而 Claude Code 这类客户端
# 说的是 Anthropic 的 Messages API。所以两个方向都要转：
#   请求：Anthropic → OpenAI（system / 内容块 / 图片 / 工具调用）
#   响应：OpenAI → Anthropic（content 块、stop_reason、usage 字段名、SSE 事件）
# 只转请求不转响应的话，客户端拿到的是 choices 结构，直接解析失败 ——
# 那等于这个端点对外声称支持、实际不能用。
# ---------------------------------------------------------------------------

# 这些请求参数两家是通用的，原样透传（名字一样、含义一样）
ANTHROPIC_PASSTHROUGH = ("temperature", "top_p", "stop", "presence_penalty",
                         "frequency_penalty", "seed", "user", "n")

# Anthropic 的 stop_reason → OpenAI 的 finish_reason（请求方向不用，反向用）
OPENAI_STOP_TO_ANTHROPIC = {
    "stop": "end_turn",
    "length": "max_tokens",
    "tool_calls": "tool_use",
    "function_call": "tool_use",
    "content_filter": "end_turn",
}


def _anthropic_system_text(system: Any) -> str:
    """Anthropic 的 system 既可以是字符串，也可以是内容块数组。"""
    if isinstance(system, str):
        return system
    if isinstance(system, list):
        parts: list[str] = []
        for block in system:
            if isinstance(block, str):
                parts.append(block)
            elif isinstance(block, dict) and block.get("type") == "text":
                parts.append(str(block.get("text") or ""))
        return "\n".join(p for p in parts if p)
    return ""


def _anthropic_image_to_openai(block: dict) -> Optional[dict]:
    """Anthropic 的 image 块 → OpenAI 的 image_url 块。"""
    source = block.get("source")
    if not isinstance(source, dict):
        return None
    stype = source.get("type")
    if stype == "base64":
        media = str(source.get("media_type") or "image/png")
        data = source.get("data") or ""
        if not data:
            return None
        return {"type": "image_url",
                "image_url": {"url": f"data:{media};base64,{data}"}}
    if stype == "url":
        url = source.get("url")
        if not url:
            return None
        return {"type": "image_url", "image_url": {"url": str(url)}}
    return None


def anthropic_tools_to_openai(tools: Any) -> list[dict]:
    """Anthropic 的 tools → OpenAI 的 tools（都是 function 风格，字段名不同）。"""
    out: list[dict] = []
    if not isinstance(tools, list):
        return out
    for tool in tools:
        if not isinstance(tool, dict):
            continue
        name = str(tool.get("name") or "")
        if not name:
            continue
        out.append({
            "type": "function",
            "function": {
                "name": name,
                "description": str(tool.get("description") or ""),
                # Anthropic 用的是 input_schema，OpenAI 叫 parameters
                "parameters": tool.get("input_schema") or {"type": "object", "properties": {}},
            },
        })
    return out


def anthropic_tool_choice_to_openai(choice: Any) -> Any:
    """Anthropic 的 tool_choice → OpenAI 的 tool_choice。

    auto→auto；any→required（必须调一个）；tool→指定某个函数。
    """
    if not isinstance(choice, dict):
        return None
    ctype = choice.get("type")
    if ctype == "auto":
        return "auto"
    if ctype == "any":
        return "required"
    if ctype == "none":
        return "none"
    if ctype == "tool":
        name = str(choice.get("name") or "")
        if name:
            return {"type": "function", "function": {"name": name}}
    return None


def anthropic_to_openai(body: dict) -> tuple[dict, list[str]]:
    """把 Anthropic /v1/messages 的请求体转成 OpenAI /v1/chat/completions 的请求体。

    返回 (payload, notes)。notes 是「有哪些东西没法完整表达」的说明，
    会被记进请求流水 —— 静默丢字段是最难查的那种问题，宁可留下痕迹。
    """
    notes: list[str] = []
    msgs: list[dict] = []

    system_text = _anthropic_system_text(body.get("system"))
    if system_text:
        msgs.append({"role": "system", "content": system_text})

    for m in body.get("messages") or []:
        if not isinstance(m, dict):
            continue
        role = str(m.get("role") or "user")
        content = m.get("content")

        if isinstance(content, str):
            msgs.append({"role": role, "content": content})
            continue
        if not isinstance(content, list):
            msgs.append({"role": role, "content": ""})
            continue

        # 内容块数组。一个 Anthropic 消息可能展开成多个 OpenAI 消息：
        # 比如 user 消息里夹着 tool_result，那在 OpenAI 那边是独立的 role=tool 消息。
        text_parts: list[dict] = []      # 文本/图片，合成一个 content 数组
        tool_calls: list[dict] = []      # assistant 的工具调用
        tool_results: list[dict] = []    # role=tool 的返回
        thinking_parts: list[str] = []

        for block in content:
            if not isinstance(block, dict):
                if isinstance(block, str):
                    text_parts.append({"type": "text", "text": block})
                continue
            btype = block.get("type")

            if btype == "text":
                text_parts.append({"type": "text", "text": str(block.get("text") or "")})
            elif btype == "image":
                img = _anthropic_image_to_openai(block)
                if img is not None:
                    text_parts.append(img)
                else:
                    notes.append("有 1 张图片没能转换（source 格式不认识），已忽略")
            elif btype == "tool_use":
                args = block.get("input")
                try:
                    arg_text = json.dumps(args if args is not None else {},
                                          ensure_ascii=False)
                except Exception:  # noqa: BLE001
                    arg_text = "{}"
                tool_calls.append({
                    "id": str(block.get("id") or f"call_{uuid.uuid4().hex[:12]}"),
                    "type": "function",
                    "function": {"name": str(block.get("name") or ""),
                                 "arguments": arg_text},
                })
            elif btype == "tool_result":
                result = block.get("content")
                if isinstance(result, list):
                    chunks: list[str] = []
                    for part in result:
                        if isinstance(part, dict) and part.get("type") == "text":
                            chunks.append(str(part.get("text") or ""))
                        elif isinstance(part, dict) and part.get("type") == "image":
                            notes.append("工具返回里的图片没法转，已忽略")
                        elif isinstance(part, str):
                            chunks.append(part)
                    result_text = "\n".join(chunks)
                elif isinstance(result, str):
                    result_text = result
                else:
                    result_text = "" if result is None else json.dumps(result, ensure_ascii=False)
                tool_results.append({
                    "role": "tool",
                    "tool_call_id": str(block.get("tool_use_id") or ""),
                    "content": result_text,
                })
            elif btype == "thinking":
                # 思维链：记下来但**不**回传给上游（上游多半不认这个字段），
                # 客户端下次带回来的 thinking 块也只做记录。
                thinking_parts.append(str(block.get("thinking") or ""))
            else:
                notes.append(f"未知内容块 {btype!r} 已忽略")

        # 组装顺序：先 tool 返回（它们对应上一条 assistant 的调用），
        # 再是普通内容。这样 OpenAI 那边的对话顺序才是对的。
        for tr in tool_results:
            if not tr["tool_call_id"]:
                notes.append("有条 tool_result 没带 tool_use_id，上游可能配不上")
            msgs.append(tr)

        if tool_calls:
            out_msg: dict = {"role": role, "tool_calls": tool_calls}
            # 有工具调用时 content 仍要带上（可能是空串）
            out_msg["content"] = _blocks_to_content(text_parts) if text_parts else None
            msgs.append(out_msg)
        elif text_parts:
            msgs.append({"role": role, "content": _blocks_to_content(text_parts)})

    payload: dict = {
        "model": str(body.get("model") or ""),
        "messages": msgs,
    }
    # max_tokens 两家同名；Anthropic 必填，缺了给个稳妥的默认值
    payload["max_tokens"] = body.get("max_tokens") or 4096
    if body.get("stream") is not None:
        payload["stream"] = bool(body.get("stream"))

    tools = anthropic_tools_to_openai(body.get("tools"))
    if tools:
        payload["tools"] = tools
        tc = anthropic_tool_choice_to_openai(body.get("tool_choice"))
        if tc is not None:
            payload["tool_choice"] = tc
    elif body.get("tool_choice"):
        notes.append("传了 tool_choice 但没传 tools，已忽略")

    for key in ANTHROPIC_PASSTHROUGH:
        if key in body and body[key] is not None:
            payload[key] = body[key]

    # 流式时主动要 usage：Anthropic 客户端要看 token 数，而上游默认
    # 不在流里带 usage（除非显式要求），不注入的话最后只能报 0。
    if payload.get("stream"):
        payload["stream_options"] = {"include_usage": True}

    for key in ("top_k", "metadata", "thinking", "context_management"):
        if body.get(key):
            notes.append(f"Anthropic 的 {key} 上游不支持，已忽略")
    return payload, notes


def _blocks_to_content(parts: list[dict]) -> Any:
    """内容块数组 → OpenAI 的 content。只有纯文本时退化成字符串，
    兼容那些不认数组形式的老上游。"""
    if all(p.get("type") == "text" for p in parts):
        return "".join(str(p.get("text") or "") for p in parts)
    return parts


def _strict_int(value: Any) -> Optional[int]:
    """usage 里的数字：只接受真正的数字，字符串数字也认，其余当没有。"""
    if isinstance(value, bool):
        return None
    if isinstance(value, (int, float)):
        return int(value)
    if isinstance(value, str):
        try:
            return int(float(value))
        except (TypeError, ValueError):
            return None
    return None


def openai_to_anthropic_response(body: bytes, model: str,
                                 fallback_id: str = "") -> bytes:
    """把上游的 OpenAI 非流式响应转成 Anthropic Messages 响应。

    上游返回的不是 JSON（网关 HTML 错误页之类）时原样返回，让调用方
    自己决定怎么处理 —— 在这里硬造一个 Anthropic 结构反而会掩盖真实错误。
    """
    try:
        obj = json.loads(body)
    except Exception:
        return body
    if not isinstance(obj, dict):
        return body

    choice: dict = {}
    choices = obj.get("choices")
    if isinstance(choices, list) and choices and isinstance(choices[0], dict):
        choice = choices[0]
    message = choice.get("message") if isinstance(choice.get("message"), dict) else {}

    content: list[dict] = []
    text = message.get("content")
    if isinstance(text, str) and text:
        content.append({"type": "text", "text": text})
    elif isinstance(text, list):
        for part in text:
            if isinstance(part, dict) and part.get("type") == "text":
                content.append({"type": "text", "text": str(part.get("text") or "")})

    for call in (message.get("tool_calls") or []):
        if not isinstance(call, dict):
            continue
        fn = call.get("function") if isinstance(call.get("function"), dict) else {}
        raw_args = fn.get("arguments")
        try:
            parsed_args = json.loads(raw_args) if isinstance(raw_args, str) and raw_args else {}
        except Exception:  # noqa: BLE001
            # 参数不是合法 JSON：Anthropic 要求 input 是对象，
            # 塞个原始串进去至少不丢信息，客户端能看见到底收到了什么。
            parsed_args = {"_raw_arguments": raw_args}
        if not isinstance(parsed_args, dict):
            parsed_args = {"_raw_arguments": parsed_args}
        content.append({
            "type": "tool_use",
            "id": str(call.get("id") or f"toolu_{uuid.uuid4().hex[:20]}"),
            "name": str(fn.get("name") or ""),
            "input": parsed_args,
        })

    if not content:
        # Anthropic 的 content 不允许为空数组，至少给一个空文本块
        content.append({"type": "text", "text": ""})

    usage = extract_usage(obj)
    finish = choice.get("finish_reason")
    stop_reason = OPENAI_STOP_TO_ANTHROPIC.get(str(finish), None) if finish else None
    if stop_reason is None and content and content[-1].get("type") == "tool_use":
        stop_reason = "tool_use"

    out = {
        "id": str(obj.get("id") or fallback_id or f"msg_{uuid.uuid4().hex[:24]}"),
        "type": "message",
        "role": "assistant",
        "model": model,
        # 顺序不能反：Anthropic 的约定是 input 在前、output 在后
        "content": content,
        "stop_reason": stop_reason or "end_turn",
        "stop_sequence": None,
        "usage": {
            "input_tokens": _strict_int(usage.get("input")) or 0,
            "output_tokens": _strict_int(usage.get("output")) or 0,
        },
    }
    cache_read = _strict_int(usage.get("cache_read"))
    cache_write = _strict_int(usage.get("cache_write"))
    if cache_read:
        out["usage"]["cache_read_input_tokens"] = cache_read
    if cache_write:
        out["usage"]["cache_creation_input_tokens"] = cache_write
    try:
        return json.dumps(out, ensure_ascii=False).encode("utf-8")
    except Exception:  # noqa: BLE001
        return body


def openai_error_to_anthropic(body: bytes, status: int) -> bytes:
    """错误体也换成 Anthropic 的形状，否则客户端的报错信息会很难看。"""
    message = ""
    etype = ""
    try:
        obj = json.loads(body)
        if isinstance(obj, dict):
            err = obj.get("error")
            if isinstance(err, dict):
                message = str(err.get("message") or "")
                etype = str(err.get("type") or "")
            elif isinstance(err, str):
                message = err
            if not message:
                message = str(obj.get("message") or "")
    except Exception:  # noqa: BLE001
        message = body[:300].decode("utf-8", "replace") if body else ""

    if not message:
        message = f"上游返回 HTTP {status}"
    # Anthropic 的错误类型是有限几个，把上游那套映射过去
    mapping = {
        400: ("invalid_request_error", "invalid_request_error"),
        401: ("authentication_error", "authentication_error"),
        403: ("permission_error", "permission_error"),
        404: ("not_found_error", "not_found_error"),
        413: ("request_too_large", "invalid_request_error"),
        429: ("rate_limit_error", "rate_limit_error"),
        500: ("api_error", "api_error"),
        529: ("overloaded_error", "overloaded_error"),
    }
    atype, _ = mapping.get(status, ("api_error", "api_error"))
    out = {"type": "error", "error": {"type": atype, "message": message}}
    try:
        return json.dumps(out, ensure_ascii=False).encode("utf-8")
    except Exception:  # noqa: BLE001
        return body


def estimate_tokens(text: str) -> int:
    """粗略估 token 数。

    这里没有上游的分词器（也不该为了估个数去加载一个几百 MB 的 BPE 表），
    所以按经验估算：英文约 4 字符一个 token，中文约 1.5 字符一个。
    真实的 count_tokens 各家算法不同，这个值只用于「大概多少」的参考，
    比如客户端做上下文压缩判断；实际计费/统计一律以流水里上游回报的为准。
    """
    if not text:
        return 0
    cjk = 0
    other = 0
    for ch in text:
        if "\u4e00" <= ch <= "\u9fff" or "\u3040" <= ch <= "\u30ff" \
                or "\uac00" <= ch <= "\ud7af":
            cjk += 1
        else:
            other += 1
    return int(cjk / 1.5 + other / 4) + 1


def count_anthropic_tokens(body: dict) -> int:
    """给 /v1/messages/count_tokens 用：把请求里所有文本加起来估一下。"""
    chunks: list[str] = [_anthropic_system_text(body.get("system"))]
    for m in body.get("messages") or []:
        if not isinstance(m, dict):
            continue
        content = m.get("content")
        if isinstance(content, str):
            chunks.append(content)
        elif isinstance(content, list):
            for block in content:
                if not isinstance(block, dict):
                    continue
                btype = block.get("type")
                if btype == "text":
                    chunks.append(str(block.get("text") or ""))
                elif btype == "tool_use":
                    chunks.append(str(block.get("name") or ""))
                    try:
                        chunks.append(json.dumps(block.get("input") or {}, ensure_ascii=False))
                    except Exception:  # noqa: BLE001
                        pass
                elif btype == "tool_result":
                    result = block.get("content")
                    if isinstance(result, str):
                        chunks.append(result)
                    elif isinstance(result, list):
                        for part in result:
                            if isinstance(part, dict) and part.get("type") == "text":
                                chunks.append(str(part.get("text") or ""))
    for tool in (body.get("tools") or []):
        if isinstance(tool, dict):
            chunks.append(str(tool.get("name") or ""))
            chunks.append(str(tool.get("description") or ""))
            try:
                chunks.append(json.dumps(tool.get("input_schema") or {}, ensure_ascii=False))
            except Exception:  # noqa: BLE001
                pass
    return estimate_tokens("\n".join(c for c in chunks if c))


class AnthropicStreamConverter:
    """OpenAI 的 SSE 分片 → Anthropic 的 SSE 事件序列。

    上游流里长这样（每条 data 一个 chunk）：
        {"choices":[{"delta":{"content":"你"}}]}
        {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"f","arguments":"{"}}]}}]}
    客户端要的是这样：
        event: message_start  / content_block_start / content_block_delta …
    逐块转，同时处理跨分片断行（SSE 的一行可能被切成两半）。
    """

    def __init__(self, model: str, msg_id: str = ""):
        self.model = model
        self.msg_id = msg_id or f"msg_{uuid.uuid4().hex[:24]}"
        self.buf = b""
        self.started = False
        self.finished = False
        self.block_index = -1          # 当前正在写的内容块序号
        self.block_kind: Optional[str] = None   # "text" / "tool_use"
        # OpenAI 的 tool_calls 用 index 区分同一个回复里的多个调用，
        # 这里记 OpenAI index → Anthropic block index 的映射
        self.tool_blocks: dict[int, int] = {}
        self.usage: dict = {}
        self.stop_reason: Optional[str] = None
        self.saw_tool_use = False

    # ---- 事件拼装 ----
    def _event(self, name: str, payload: dict) -> bytes:
        data = json.dumps(payload, ensure_ascii=False, separators=(",", ":"))
        return f"event: {name}\ndata: {data}\n\n".encode("utf-8")

    def _message_start(self) -> bytes:
        self.started = True
        return self._event("message_start", {
            "type": "message_start",
            "message": {
                "id": self.msg_id,
                "type": "message",
                "role": "assistant",
                "model": self.model,
                "content": [],
                "stop_reason": None,
                "stop_sequence": None,
                # 此刻还不知道真实用量（OpenAI 流末尾才给），
                # 最终数字在 message_delta 里补齐
                "usage": {"input_tokens": 0, "output_tokens": 0},
            },
        })

    def _close_block(self) -> bytes:
        if self.block_index < 0 or self.block_kind is None:
            return b""
        out = self._event("content_block_stop",
                          {"type": "content_block_stop", "index": self.block_index})
        self.block_kind = None
        return out

    def _open_text_block(self) -> bytes:
        if self.block_kind == "text":
            return b""
        out = self._close_block()
        self.block_index += 1
        self.block_kind = "text"
        out += self._event("content_block_start", {
            "type": "content_block_start",
            "index": self.block_index,
            "content_block": {"type": "text", "text": ""},
        })
        return out

    def _open_tool_block(self, oai_index: int, tool_id: str, name: str) -> bytes:
        out = self._close_block()
        self.block_index += 1
        self.block_kind = "tool_use"
        self.saw_tool_use = True
        self.tool_blocks[oai_index] = self.block_index
        out += self._event("content_block_start", {
            "type": "content_block_start",
            "index": self.block_index,
            "content_block": {
                "type": "tool_use",
                "id": tool_id or f"toolu_{uuid.uuid4().hex[:20]}",
                "name": name,
                # Anthropic 要求这里给空对象，实际参数走后续的 input_json_delta
                "input": {},
            },
        })
        return out

    def _finish(self) -> bytes:
        if self.finished:
            return b""
        self.finished = True
        out = self._close_block()
        stop = self.stop_reason
        if stop is None:
            stop = "tool_use" if self.saw_tool_use else "end_turn"
        out += self._event("message_delta", {
            "type": "message_delta",
            "delta": {"stop_reason": stop, "stop_sequence": None},
            "usage": {"output_tokens": _strict_int(self.usage.get("output")) or 0},
        })
        out += self._event("message_stop", {"type": "message_stop"})
        return out

    # ---- 主入口 ----
    def feed(self, data: bytes, final: bool = False) -> bytes:
        if self.finished:
            return b""
        self.buf += data
        if final:
            block, self.buf = self.buf, b""
        else:
            idx = self.buf.rfind(b"\n")
            if idx < 0:
                return b""
            block, self.buf = self.buf[: idx + 1], self.buf[idx + 1:]
        if not block and not final:
            return b""
        return self._convert(block)

    def _convert(self, block: bytes) -> bytes:
        out = b""
        for line in block.split(b"\n"):
            stripped = line.strip()
            if not stripped.startswith(b"data:"):
                continue
            payload = stripped[5:].strip()
            if not payload or payload == b"[DONE]":
                continue
            try:
                obj = json.loads(payload)
            except Exception:  # noqa: BLE001
                continue
            if not isinstance(obj, dict):
                continue

            if not self.started:
                out += self._message_start()

            got_usage = extract_usage(obj)
            if got_usage:
                self.usage = got_usage

            choices = obj.get("choices")
            if not isinstance(choices, list) or not choices:
                continue
            first = choices[0]
            if not isinstance(first, dict):
                continue

            if first.get("finish_reason"):
                self.stop_reason = OPENAI_STOP_TO_ANTHROPIC.get(
                    str(first["finish_reason"]), self.stop_reason)

            delta = first.get("delta")
            if not isinstance(delta, dict):
                delta = first.get("message") if isinstance(first.get("message"), dict) else {}

            text = delta.get("content")
            if isinstance(text, str) and text:
                out += self._open_text_block()
                out += self._event("content_block_delta", {
                    "type": "content_block_delta",
                    "index": self.block_index,
                    "delta": {"type": "text_delta", "text": text},
                })
            elif isinstance(text, list):
                for part in text:
                    if isinstance(part, dict) and part.get("type") == "text" and part.get("text"):
                        out += self._open_text_block()
                        out += self._event("content_block_delta", {
                            "type": "content_block_delta",
                            "index": self.block_index,
                            "delta": {"type": "text_delta", "text": str(part["text"])},
                        })

            for call in (delta.get("tool_calls") or []):
                if not isinstance(call, dict):
                    continue
                oai_index = call.get("index")
                if not isinstance(oai_index, int):
                    oai_index = len(self.tool_blocks)
                fn = call.get("function") if isinstance(call.get("function"), dict) else {}
                if oai_index not in self.tool_blocks:
                    out += self._open_tool_block(oai_index, str(call.get("id") or ""),
                                                 str(fn.get("name") or ""))
                else:
                    self.block_kind = "tool_use"   # 回到这个工具块继续写参数
                    self.block_index = self.tool_blocks[oai_index]
                args = fn.get("arguments")
                if isinstance(args, str) and args:
                    out += self._event("content_block_delta", {
                        "type": "content_block_delta",
                        "index": self.tool_blocks.get(oai_index, self.block_index),
                        "delta": {"type": "input_json_delta", "partial_json": args},
                    })
        return out

    def close(self) -> bytes:
        """上游流结束：补上收尾事件。"""
        out = b""
        if not self.started:
            # 上游一个字都没吐，也得给客户端一个结构完整的空回复
            out += self._message_start()
        out += self._finish()
        return out

    def error(self, message: str, atype: str = "api_error") -> bytes:
        """已经发出 message_start 之后才出错：按 Anthropic 的约定，
        在流里发一个 error 事件收尾（而不是直接断连）。"""
        return self._event("error", {
            "type": "error",
            "error": {"type": atype, "message": message},
        })


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

    # 请求体缓存：审计要先读一次，业务逻辑还要再读 —— HTTP 流只能读一次
    _body_cache: Optional[bytes] = None
    _audit_body: Optional[bytes] = None

    def setup(self) -> None:
        super().setup()
        self._body_cache = None
        self._audit_body = None
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
        """把客户端断开当成正常事件，别刷一堆 traceback。

        另外：HTTP/1.1 长连接下，同一个 handler 实例会连续处理多条请求，
        请求体缓存 _body_cache / _audit_body 必须在每条请求开头清空。
        只在 setup 里清是不够的 —— setup 每条连接只跑一次，
        第二条请求会读到上一条缓存的 body，表现为「点了没反应 / 要点很多次」，
        而且真正的 body 还留在 socket 里，会把下一条请求也搞坏。
        """
        self._body_cache = None
        self._audit_body = None
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
                query = urllib.parse.parse_qs(urllib.parse.urlsplit(self.path).query)
                rng = (query.get("range") or [""])[0]
                out = self.router.snapshot()
                if rng:
                    # 跨重启的聚合统计（今天 / 本月 / 全部），不会随进程清零
                    out["stats"] = self.router.stats.meta()
                    out["stats_range"] = self.router.stats.snap_range(rng)
                return self._send_json(200, out)
            if path == "/admin/history":
                return self._send_json(200, {"history": list(self.router.history)})
            if path == "/admin/admin-log":
                query = urllib.parse.parse_qs(urllib.parse.urlsplit(self.path).query)
                try:
                    limit = int((query.get("limit") or ["200"])[0])
                except (TypeError, ValueError):
                    limit = 200
                return self._send_json(200, {"log": self.router.read_admin_log(limit)})
            if path == "/admin/journal/files":
                return self._send_json(200, {"files": [
                    {"path": os.path.basename(p),
                     "size": (os.path.getsize(p) if os.path.exists(p) else 0)}
                    for p in self.router.stats.journal_files()]})
            if path == "/admin/journal/entry":
                # 单条完整记录（含输入输出正文），用户展开详情时才取
                q = urllib.parse.parse_qs(urllib.parse.urlsplit(self.path).query)
                at = (q.get("at") or [""])[0]
                upstream = (q.get("upstream") or [""])[0]
                entry = self.router.stats.find_journal_entry(at, upstream)
                if entry is None:
                    return self._send_json(404, {"error": {"message": "找不到这条记录"}})
                return self._send_json(200, {"entry": entry})
            if path == "/admin/journal/index":
                # 流水里有数据的日期/月/分组以及各自条数（前端做筛选下拉）
                return self._send_json(200, self.router.stats.journal_index())
            if path == "/admin/journal":
                # 全量流水翻页：按天/月/分组筛，条件是查询参数
                q = urllib.parse.parse_qs(urllib.parse.urlsplit(self.path).query)

                def one(key, default=""):
                    return (q.get(key) or [default])[0]

                return self._send_json(200, self.router.stats.query_journal(
                    day=one("day"), month=one("month"), group=one("group"),
                    upstream=one("upstream"),
                    offset=one("offset", "0"), limit=one("limit", "100"),
                    with_text=one("with_text", "") in ("1", "true", "yes")))
            if path == "/admin/state/versions":
                return self._send_json(200, {"versions": self.router.list_state_versions()})

            handlers = {
                "/admin/upstream/add": self._h_upstream_add,
                "/admin/upstream/update": self._h_upstream_update,
                "/admin/upstream/delete": self._h_upstream_delete,
                "/admin/upstream/capacity": self._h_upstream_capacity,
                "/admin/fetch-models": self._h_fetch_models,
                "/admin/select-models": self._h_select_models,
                "/admin/set-group": self._h_set_group,
                "/admin/rename-group": self._h_rename_group,
                "/admin/model/save": self._h_model_save,
                "/admin/model/delete": self._h_model_delete,
                "/admin/model/orderings": self._h_model_orderings,
                "/admin/model/order/save": self._h_order_save,
                "/admin/model/order/apply": self._h_order_apply,
                "/admin/model/order/delete": self._h_order_delete,
                "/admin/model/reorder-by-speed": self._h_reorder_by_speed,
                "/admin/model/quota": self._h_model_quota,
                "/admin/auto-configure": self._h_auto_configure,
                "/admin/speedtest": self._h_speedtest,
                "/admin/reset": self._h_reset,
                "/admin/reset-to-config": self._h_reset_to_config,
                "/admin/state/restore": self._h_state_restore,
                "/admin/journal/rotate": self._h_journal_rotate,
                "/admin/stats/rebuild": self._h_stats_rebuild,
            }
            if path in handlers:
                if method != "POST":
                    return self._send_json(405, {"error": {"message": f"{path} 需要 POST"}})
                # 改配置的操作都要留痕：谁、什么时候、来自哪里、改成了什么
                self._audit(path)
                return handlers[path]()

            if path.startswith("/v1/"):
                if path == "/v1/models" and method == "GET":
                    return self._handle_models()
                if path == "/v1/messages" and method == "POST":
                    return self._handle_anthropic_messages()
                if path == "/v1/messages/count_tokens" and method == "POST":
                    return self._handle_count_tokens()
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

    # 哪些管理接口前后会改变状态，值得记一笔审计
    AUDIT_DETAIL_KEYS = ("id", "name", "upstream", "model", "models", "from", "to",
                         "enabled", "mapping", "entries", "label", "strategy",
                         "max_concurrent", "max_per_minute")

    def _audit(self, path: str) -> None:
        """把这次管理调用记进审计日志（读 body 前先缓存，免得流被消费掉）。"""
        if self._audit_body is None:
            try:
                raw = self._read_body()
            except Exception:  # noqa: BLE001
                raw = b""
            self._audit_body = raw
        detail: dict = {}
        try:
            if self._audit_body:
                parsed = json.loads(self._audit_body)
                if isinstance(parsed, dict):
                    for key in self.AUDIT_DETAIL_KEYS:
                        if key in parsed:
                            value = parsed[key]
                            # 密钥只留提示，别把真 Key 写进日志
                            if key == "api_key":
                                value = mask_key(str(value))
                            detail[key] = value
        except Exception:  # noqa: BLE001
            pass
        self.router.admin_log(path, detail, actor=self.client_address[0]
                              if self.client_address else "-")

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
        # 审计需要先读一次 body（HTTP 流只能读一次），缓存起来给业务逻辑复用
        if self._body_cache is not None:
            return self._body_cache
        self._body_cache = self._read_body_raw()
        return self._body_cache

    def _read_body_raw(self) -> bytes:
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

    def _h_model_orderings(self) -> None:
        """列出某个分组的所有具名排序。"""
        def go():
            payload = self._read_json_body()
            return self.router.list_orderings(str(payload.get("name") or ""))
        status, out = self._result(go)
        self._send_json(status, out)

    def _h_order_save(self) -> None:
        """把一套顺序存成具名排序；body 里带 order 就顺带把它应用上去。"""
        def go():
            payload = self._read_json_body()
            order = payload.get("order")
            if order is not None and not isinstance(order, list):
                raise ValueError("order 必须是数组")
            return self.router.save_ordering(
                str(payload.get("name") or ""), str(payload.get("label") or ""), order)
        status, out = self._result(go)
        self._send_json(status, out)

    def _h_order_apply(self) -> None:
        """切换到某套已保存的排序。"""
        def go():
            payload = self._read_json_body()
            return self.router.apply_ordering(
                str(payload.get("name") or ""), str(payload.get("label") or ""))
        status, out = self._result(go)
        self._send_json(status, out)

    def _h_order_delete(self) -> None:
        def go():
            payload = self._read_json_body()
            return self.router.delete_ordering(
                str(payload.get("name") or ""), str(payload.get("label") or ""))
        status, out = self._result(go)
        self._send_json(status, out)

    def _h_reorder_by_speed(self) -> None:
        """按测速结果重排某个分组，并保存成一套具名排序。"""
        def go():
            payload = self._read_json_body()
            name = str(payload.get("name") or "")
            label = str(payload.get("label") or "按测速")
            results = payload.get("results")
            return self.router.reorder_by_speed(name, label, results)
        status, out = self._result(go)
        self._send_json(status, out)

    def _h_model_quota(self) -> None:
        """设置某个分组里各上游的调用额度（0 = 不调用它）。

        两种写法都认：
          {"name":"deepseek","upstream":"u1","model":"ds1","quota":0}     单条
          {"name":"deepseek","entries":[{"upstream":"u1","quota":0}, ...]} 批量
        """
        def go():
            payload = self._read_json_body()
            name = str(payload.get("name") or "")
            raw_entries = payload.get("entries")
            if raw_entries is not None and not isinstance(raw_entries, list):
                raise ValueError("entries 必须是数组")
            if raw_entries is not None and not raw_entries:
                raise ValueError("entries 不能为空")
            items = [e for e in (raw_entries or []) if isinstance(e, dict)]
            if not items:
                # 单条写法：把整份 payload 当成一条
                items = [{"upstream": payload.get("upstream"),
                          "model": payload.get("model"),
                          "quota": payload.get("quota")}]
            return self.router.set_quota(name, items)
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

    def _h_upstream_capacity(self) -> None:
        """设置某个上游的并发/速率上限（0 或空 = 不限量）。"""
        def go():
            payload = self._read_json_body()
            uid = str(payload.get("id") or "")
            if not uid:
                raise ValueError("缺少 id")
            return self.router.set_capacity(
                uid,
                payload.get("max_concurrent"),
                payload.get("max_per_minute"),
            )
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
            # tests: [{"id": 上游, "model": 该上游上的真实模型名}] —— 精确测某个模型
            # 模型路由弹窗里每行一个「测试」按钮走这条路径。
            tests = payload.get("tests")
            if tests is not None:
                if not isinstance(tests, list):
                    raise ValueError("tests 必须是数组")
                results: list[dict] = []
                for item in tests:
                    if not isinstance(item, dict):
                        continue
                    up = self.router.find_upstream(str(item.get("id") or ""))
                    if up is None:
                        continue
                    model = item.get("model")
                    try:
                        results.append(up.ping(str(model) if model else None))
                    except Exception as exc:  # noqa: BLE001
                        results.append({"id": up.id, "name": up.name, "ok": False,
                                        "latency_seconds": None,
                                        "error": f"{type(exc).__name__}: {exc}"})
                self.router._save_state()
                return {"ok": True, "results": results}
            ids = payload.get("ids")
            if ids is not None and not isinstance(ids, list):
                raise ValueError("ids 必须是数组")
            targets = [u for u in self.router.upstreams
                       if u.enabled and (not ids or u.id in set(str(i) for i in ids))]
            if not targets:
                return {"ok": True, "results": [], "upstreams": []}
            # 每个上游：对它**勾选了的每个模型**各测一次(一个上游勾了多个
            # 模型时，各模型的连通性/延迟不一样，只测第一个会骗人)。
            # 一个都没勾选时退回第一个可用模型，至少给个基本判断。
            jobs: list[tuple[Upstream, str]] = []
            for u in targets:
                models = list(u.selected())
                if not models:
                    models = list(u.available_models[:1])
                for m in models:
                    jobs.append((u, m))
            if not jobs:
                return {"ok": True, "results": [], "upstreams": []}
            results: list[dict] = []
            with concurrent.futures.ThreadPoolExecutor(
                    max_workers=min(8, len(jobs))) as pool:
                futures = {pool.submit(u.ping, m): (u, m) for u, m in jobs}
                for fut in concurrent.futures.as_completed(futures, timeout=180):
                    u, m = futures[fut]
                    try:
                        results.append(fut.result())
                    except Exception as exc:  # noqa: BLE001
                        results.append({"id": u.id, "name": u.name, "model": m,
                                        "ok": False, "latency_seconds": None,
                                        "error": f"{type(exc).__name__}: {exc}"})
            order = {u.id: i for i, u in enumerate(self.router.upstreams)}
            results.sort(key=lambda r: (order.get(r.get("id", ""), 999),
                                         str(r.get("model") or "")))
            # 每个上游的汇总(取第一个成功的延迟)——给「按测速重排」和卡片汇总用
            summary: dict[str, dict] = {}
            for r in results:
                uid = r.get("id")
                if uid not in summary:
                    summary[uid] = {"id": uid, "name": r.get("name"),
                                    "ok": False, "latency_seconds": None,
                                    "rate_limited": False, "model_count": 0}
                s = summary[uid]
                s["model_count"] += 1
                if r.get("ok"):
                    s["ok"] = True
                    lat = r.get("latency_seconds")
                    if lat is not None and (s["latency_seconds"] is None
                                             or lat < s["latency_seconds"]):
                        s["latency_seconds"] = lat
                elif r.get("rate_limited"):
                    s["rate_limited"] = True
            # 连通性结果要持久化(重启后仍能看到上次测试)
            self.router._save_state()
            return {"ok": True, "results": results,
                    "upstreams": list(summary.values())}
        status, out = self._result(go)
        self._send_json(status, out)

    def _h_reset(self) -> None:
        """已停用：不再清除统计和流水。

        只允许「冷却复位」这种不丢数据的操作（上游失败计数归零，
        让它立刻重新参与路由）—— 统计和请求流水一律保留。
        """
        for u in self.router.upstreams:
            u.reset_stats()
        self.router.rr_counters.clear()
        self.router.admin_log("reset-cooldown-only", {"note": "仅复位冷却，不动统计"})
        self._send_json(200, {"ok": True, "message": "冷却与失败计数已复位（统计和流水保留）"})

    def _h_reset_to_config(self) -> None:
        status, out = self._result(self.router.reset_to_config)
        self._send_json(status, out)

    def _h_state_restore(self) -> None:
        """按快照文件名回退状态（回退前会先把当前状态存成快照，可再退回来）。"""
        def go():
            payload = self._read_json_body()
            return self.router.restore_state_version(str(payload.get("file") or ""))
        status, out = self._result(go)
        self._send_json(status, out)

    def _h_journal_rotate(self) -> None:
        """把当前流水归档（改名保留），立刻另起一份空的接着写。"""
        def go():
            archived = self.router.stats.rotate_journal_now()
            return {"ok": True, "archived": os.path.basename(archived) if archived else None,
                    "note": "归档文件保留，不会被删除"}
        status, out = self._result(go)
        self._send_json(status, out)

    def _h_stats_rebuild(self) -> None:
        """以流水为准，彻底重算累计统计（聚合落后于流水时用）。"""
        status, out = self._result(self.router.stats.rebuild_stats_from_journal)
        self._send_json(status, out)

    # ---- 历史记录 ----
    def _record_history(self, upstream: Optional[Upstream], requested_model: Optional[str],
                        effective_model: str, status: int, elapsed: float,
                        error: Optional[str] = None, stream: bool = False,
                        attempts: int = 1, route_source: str = "",
                        usage: Optional[dict] = None,
                        request_text: Optional[str] = None,
                        response_text: Optional[str] = None,
                        reasoning_text: Optional[str] = None,
                        trail: Optional[list[dict]] = None) -> None:
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
            # 这次请求依次试过的每个上游和各自结果。
            # 「上游」「ok」只是最后一家，中间失败的靠这个才看得见 ——
            # 右侧卡片记的是「每次尝试」，左侧统计要能对上就得留这份轨迹。
            "trail": trail or [],
        }
        with self.router.lock:
            self.router.history.append(entry)
            self.router.total_requests += 1
        # 落盘放在锁外，磁盘写不拖住并发的请求
        self.router.stats.record(entry)

    def _handle_count_tokens(self) -> None:
        """Anthropic 的 count_tokens：本地估算，不真去调上游。

        Claude Code 这类客户端会用它在发请求前判断上下文还能不能塞下。
        我们手里没有上游的分词器，硬要精确只能把请求先发给上游 ——
        那样每次估算都变成一次真实调用（还计费），代价太大。
        这里按经验公式估个量级，够客户端做「要不要压缩」的决策。
        """
        raw_body = self._read_body()
        try:
            body = json.loads(raw_body)
        except Exception:
            return self._send_json(400, {"type": "error", "error": {
                "type": "invalid_request_error", "message": "请求体不是合法 JSON"}})
        if not isinstance(body, dict):
            return self._send_json(400, {"type": "error", "error": {
                "type": "invalid_request_error", "message": "请求体必须是 JSON 对象"}})
        return self._send_json(200, {"input_tokens": count_anthropic_tokens(body)})

    # ---- 核心代理 ----
    def _handle_anthropic_messages(self) -> None:
        """Anthropic Messages API（/v1/messages）→ OpenAI Chat Completions。

        请求和响应两个方向都要转：上游只认 OpenAI 格式，客户端说的是
        Anthropic 格式。只转一半的话客户端会拿到 choices 结构而解析失败。
        """
        raw_body = self._read_body()
        try:
            body = json.loads(raw_body)
        except Exception:
            return self._send_json(400, {"type": "error", "error": {
                "type": "invalid_request_error", "message": "请求体不是合法 JSON"}})
        if not isinstance(body, dict):
            return self._send_json(400, {"type": "error", "error": {
                "type": "invalid_request_error", "message": "请求体必须是 JSON 对象"}})

        if not body.get("model"):
            return self._send_json(400, {"type": "error", "error": {
                "type": "invalid_request_error", "message": "缺少 model"}})
        if not body.get("messages"):
            return self._send_json(400, {"type": "error", "error": {
                "type": "invalid_request_error", "message": "缺少 messages"}})

        openai_payload, notes = anthropic_to_openai(body)
        if notes:
            # 哪些字段没能完整表达，留在日志里，免得静默丢东西查不出来
            self.log_message("Anthropic 适配提示: %s", "；".join(notes))
        self._proxy("POST", "/v1/chat/completions", custom_payload=openai_payload,
                    anthropic=True, adapt_notes=notes)

    def _proxy(self, method: str, path: str, custom_payload: Any = None,
               anthropic: bool = False,
               adapt_notes: Optional[list[str]] = None) -> None:
        if custom_payload:
            payload = custom_payload
            raw_body = json.dumps(custom_payload).encode("utf-8")
        else:
            raw_body = self._read_body() if method in ("POST", "PUT", "PATCH") else b""
            payload = None

        requested_model: Optional[str] = None
        if not payload and raw_body:
            try:
                parsed = json.loads(raw_body)
                if isinstance(parsed, dict):
                    payload = parsed
            except Exception:
                payload = None
        
        if payload:
            m = payload.get("model")
            requested_model = str(m) if m else None

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
        # 这次请求试过谁、结果如何。失败的上游也记进来，
        # 左侧统计靠它才能和右侧卡片的「请求/成功/失败」对上。
        trail: list[dict] = []
        queue_deadline = t_start + self.router.queue_timeout
        tried: set[int] = set()
        # 这一轮里因为「并发/速率满了」被挡下的上游：它们没坏，只是这会儿排不上。
        # 单独记着，等所有能试的都试完了再回头等它们腾出容量。
        cap_full: dict[int, str] = {}

        while True:
            cap_full.clear()
            for i, (up, eff_model) in enumerate(pairs):
                if i in tried:
                    continue
                lease, denied = up.acquire()
                if denied:
                    cap_full[i] = denied
                    continue
                tried.add(i)
                try:
                    self._try_upstream(up, eff_model, method, path, payload, raw_body,
                                       requested_model, route_source, attempts=len(pairs),
                                       trail=trail, anthropic=anthropic,
                                       adapt_notes=adapt_notes)
                    return
                except Retryable as exc:
                    last = exc
                    attempts_log.append((up, exc.reason))
                    trail.append({"upstream": up.name, "upstream_id": up.id, "ok": False,
                                  "model": eff_model, "status": exc.status,
                                  "reason": exc.reason})
                    remaining = len(pairs) - i - 1
                    if remaining > 0:
                        self.log_message("上游 %s 失败(%s)，换下一个（还剩 %d 个）",
                                         up.name, exc.reason, remaining)
                    continue
                finally:
                    # 流式请求要一直占着坑，所以在这里才释放 ——
                    # _try_upstream 返回时流已经转发完了。
                    lease.release()

            # 到这儿说明这一轮能试的都试完了。
            # 若剩下的全是「排不上队」且还没到等待上限，就歇一下再来一轮 ——
            # 那些上游没坏，只是此刻并发满了，等一下通常就能进去。
            if not cap_full or time.time() >= queue_deadline:
                break
            time.sleep(0.1)

        # 一条都没发出去，全卡在容量上限上：这既不是「上游坏了」也不是
        # 「没配置」，得说清楚，否则客户端会以为是自己参数不对。
        if last is None and not tried and cap_full:
            detail = "；".join(f"{pairs[i][0].name}（{why}）" for i, why in sorted(cap_full.items()))
            reason = f"所有上游都达到并发/速率上限：{detail}"
            self._record_history(None, requested_model, "-", 429,
                                 time.time() - t_start, reason,
                                 attempts=len(pairs), route_source=route_source)
            self.log_message("容量已满，%d 个上游都没排上：%s", len(cap_full), detail)
            return self._send_json(429, {"error": {
                "message": reason,
                "type": "capacity_exhausted",
                "retry_after_seconds": 1,
                "upstreams": [{"name": pairs[i][0].name, "reason": why}
                              for i, why in sorted(cap_full.items())]}})

        if last is not None:
            self._record_history(last.upstream, requested_model, "-", last.status,
                                 time.time() - t_start, last.reason,
                                 attempts=len(pairs), route_source=route_source,
                                 trail=trail)
            self.close_connection = False
            body = last.body
            try:
                json.loads(body)
            except Exception:
                body = json.dumps({"error": {
                    "message": last.reason or "上游不可用", "type": "upstream_error",
                    "upstream": last.upstream.name}}, ensure_ascii=False).encode("utf-8")
            # 全都试过还是失败：错误体也要换成客户端那边认识的形状。
            # 否则 Anthropic 客户端拿到的是上游的 new_api_error 这类结构，
            # 它只会报「解析失败」，真正的错因（Key 无效 / 限流）就看不见了。
            if anthropic:
                body = openai_error_to_anthropic(
                    body, last.status if 400 <= last.status < 600 else 502)
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
                      attempts: int, trail: Optional[list[dict]] = None,
                      anthropic: bool = False,
                      adapt_notes: Optional[list[str]] = None) -> None:
        trail = trail if trail is not None else []
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
            if anthropic:
                # 错误体也换成 Anthropic 的形状，否则客户端只会说「解析失败」，
                # 真正的错因（Key 无效 / 参数不对）就看不见了
                body = openai_error_to_anthropic(body, status)
                self._relay_json(status, resp, body, up.name, time.time() - t0,
                                 content_type="application/json")
            else:
                self._relay_json(status, resp, body, up.name, time.time() - t0)
            trail.append({"upstream": up.name, "upstream_id": up.id, "ok": False,
                          "model": eff_model, "status": status, "client_error": True,
                          "reason": f"客户端错误 HTTP {status}"})
            self._record_history(up, requested_model, eff_model or "-", status,
                                 time.time() - t0, f"客户端错误 HTTP {status}",
                                 attempts=attempts, route_source=route_source,
                                 trail=trail)
            return

        # 2.5) 200 但回来的是网页：很多网关（New API / one-api 这类）
        # 对认不出的路径不报 404，而是回 200 + 首页 HTML。
        # 这永远不是合法的 API 响应，必须当上游故障处理：换下一个，
        # 或最终回 502 + JSON 错因 —— 绝不能把 HTML 当模型回复甩给客户端
        # （要 SSE/JSON 的客户端一解析就断，而且 200 让所有状态码检查全绿）。
        # 判定以正文开头为准，标错 Content-Type 的真流式/真 JSON 不能误杀。
        if looks_like_html_page(ctype):
            try:
                head = resp.peek(256)
            except Exception:  # noqa: BLE001
                head = b""
            if _looks_like_sse_head(head):
                ctype = "text/event-stream"          # 其实是流，只是标错了
            elif (head or b"").lstrip()[:1] in (b"{", b"["):
                ctype = "application/json"           # 其实是 JSON，只是标错了
            else:
                try:
                    resp.read()
                except Exception:  # noqa: BLE001
                    pass
                up._close(conn)
                reason = "上游返回网页(HTML)而不是 API 响应"
                up.record_failure(reason, self.router.failure_threshold, up.cooldown)
                raise Retryable(up, 502, b"", reason)

        # 3) 流式
        if "text/event-stream" in ctype:
            self._relay_stream(conn, resp, up, requested_model, eff_model, t0,
                               route_source, attempts, payload, trail,
                               anthropic=anthropic)
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

        # 正文其实也是网页（上面 Content-Type 检查没抓到的那种）：同样别放行
        # 正文其实也是网页（上面 Content-Type 检查没抓到的那种）：同样别放行
        if looks_like_html_page("", body):
            reason = "上游返回网页(HTML)而不是 API 响应"
            up.record_failure(reason, self.router.failure_threshold, up.cooldown)
            raise Retryable(up, 502, b"", reason)

        if anthropic:
            # 客户端要的是 Anthropic 格式：模型名不用单独改了，
            # 转换函数会把 model 一步写成客户端请求的那个名字。
            body = openai_to_anthropic_response(body, requested_model or eff_model or "",
                                                fallback_id=resp.getheader("x-request-id") or "")
        elif eff_model and requested_model and eff_model != requested_model:
            body = rewrite_json_model(body, eff_model, requested_model)

        resp_text, usage, reasoning = response_to_text(body)
        up.record_success(latency)
        self._relay_json(status, resp, body, up.name, latency=latency,
                         effective=eff_model, content_type="application/json")
        trail.append({"upstream": up.name, "upstream_id": up.id, "ok": True,
                      "model": eff_model, "status": status, "reason": ""})
        self._record_history(up, requested_model, eff_model or "-", status, latency,
                             attempts=attempts, route_source=route_source,
                             usage=usage,
                             request_text=messages_to_text(payload),
                             response_text=resp_text,
                             reasoning_text=reasoning,
                             trail=trail)

    def _relay_json(self, status: int, resp, body: bytes, upstream_name: str,
                    latency: float = 0.0, effective: Optional[str] = None,
                    content_type: str = "") -> None:
        self.send_response(status)
        # 转成 Anthropic 结构后不能再沿用上游的 Content-Type（那边是 OpenAI 的），
        # 统一按 JSON 发，客户端只认自己那套。
        self.send_header("Content-Type",
                         content_type or resp.getheader("Content-Type") or "application/json")
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
                      attempts: int, payload: Optional[dict] = None,
                      trail: Optional[list[dict]] = None,
                      anthropic: bool = False) -> None:
        trail = trail if trail is not None else []
        headers_sent = False
        acc = make_sse_collector()      # 边转发边攒文本和 token 用量
        conv: Any = None
        try:
            if anthropic:
                # Anthropic 客户端要的是 message_start / content_block_delta 这套
                # 事件序列，不是上游那套 OpenAI chunk
                conv = AnthropicStreamConverter(requested_model or eff_model or "")
            else:
                rewriter = SSEModelRewriter(requested_model or "", eff_model or "")
                conv = rewriter

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
            self.send_header("Content-Type", "text/event-stream")
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
            out = conv.feed(first_chunk)
            if out:
                self._write_chunk(out)

            while True:
                chunk = resp.read1(262144)
                if not chunk:
                    break
                collect_sse(chunk, acc)
                out = conv.feed(chunk)
                if out:
                    self._write_chunk(out)

            if anthropic:
                tail = conv.close()
            else:
                tail = conv.feed(b"", final=True)
            if tail:
                self._write_chunk(tail)
            self.wfile.write(b"0\r\n\r\n")
            self.wfile.flush()

            latency = first_byte_at - t0
            up.record_success(latency)
            trail.append({"upstream": up.name, "upstream_id": up.id, "ok": True,
                          "model": eff_model, "status": resp.status, "reason": ""})
            self._record_history(up, requested_model, eff_model or "-", resp.status,
                                 latency, stream=True, attempts=attempts,
                                 route_source=route_source,
                                 usage=acc["usage"],
                                 request_text=messages_to_text(payload),
                                 response_text="".join(acc["text"]),
                                 reasoning_text="".join(acc["reasoning"]),
                                 trail=trail)
            self.log_message("流式：上游=%s 首字节=%.2fs 模型=%s",
                             up.name, latency, eff_model or requested_model)
        except Retryable:
            raise
        except (BrokenPipeError, ConnectionResetError):
            up.record_neutral("客户端提前断开")
            self.close_connection = True
        except Exception as exc:  # noqa: BLE001
            reason = f"流中断: {type(exc).__name__}: {exc}"
            up.record_failure(reason, self.router.failure_threshold, up.cooldown)
            self.close_connection = True
            if not headers_sent:
                raise Retryable(up, 502, b"", f"流式响应失败: {exc}") from exc
            # 响应头已经发出去了，没法再换上游，但这个失败也要留在记录里，
            # 否则右侧卡片记了失败、左侧统计却没有，两边又对不上
            if anthropic:
                # 客户端已经收到 message_start 了，按 Anthropic 的约定
                # 用一个 error 事件收尾，客户端才知道这次是失败的。
                try:
                    self._write_chunk(conv.error(reason, "api_error"))
                    self.wfile.write(b"0\r\n\r\n")
                    self.wfile.flush()
                except Exception:  # noqa: BLE001
                    pass
            trail.append({"upstream": up.name, "upstream_id": up.id, "ok": False,
                          "model": eff_model, "status": 502, "reason": reason})
            self._record_history(up, requested_model, eff_model or "-", 502,
                                 time.time() - t0, reason, stream=True,
                                 attempts=attempts, route_source=route_source,
                                 usage=acc["usage"],
                                 request_text=messages_to_text(payload),
                                 response_text="".join(acc["text"]),
                                 reasoning_text="".join(acc["reasoning"]),
                                 trail=trail)
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
