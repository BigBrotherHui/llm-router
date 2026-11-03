#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""New API / One API 站点的每日签到。

llm-router 的可选模块。刻意不 import router.py（避免循环依赖），
只接收普通 dict 和鸭子类型的上游对象，所以能单独跑、单独测。

为什么签到要另配一个令牌
    推理用的 sk-... key 只能打 /v1/*；签到在 /api/user/* 下面，走的是站点
    「个人设置 → 访问令牌」那个 token。两者不是一回事，所以签到页给每个上游
    单独存一个 access token，与上游卡片里的 API Key 各存各的。

站点布局有两种（实测）
    新版本：GET/POST /api/user/self/checkin
    老版本：GET/POST /api/user/checkin          （v1.0.0-rc.46 只有这个）
    先试新的，404/405 就换老的，并把能用的那个记进配置，下次先问它。

Cloudflare / Turnstile
    有些站点给签到挂了人机验证，访问令牌过不去，必须由浏览器完成。
    这里只识别、不绕过：认出来就把上游标记成 blocked 并短路，
    等用户自己在站点上关掉验证、或重新保存令牌之后再试。
"""

from __future__ import annotations

import json
import os
import random
import re
import socket
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
from typing import Any, Optional

__all__ = [
    "CHECKIN_PATHS", "CheckinStats", "CheckinScheduler",
    "site_root", "probe", "press", "run_one", "describe",
]

# 签到接口的两个候选路径：新的在前
CHECKIN_PATHS = ("/api/user/self/checkin", "/api/user/checkin")

DEFAULT_TIMEOUT = 15.0

# 站点大多会挡非浏览器 UA；这里跟浏览器一致，避免被当成爬虫直接 403。
USER_AGENT = ("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 "
              "(KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36")

# 统计文件保留多少条明细
STATS_KEEP = 2000

# base_url 尾部这些「只是入口前缀」的路径段要剥掉，才得到站点根。
# 例：https://x.com/v1 → https://x.com
#     https://ark.cn-beijing.volces.com/api/v3 → https://ark.cn-beijing.volces.com
#     https://superaichao.xin/openai → https://superaichao.xin
_TAIL_PREFIX_RE = re.compile(
    r"^(?:v\d+[a-z]*|openai|anthropic|gemini|api|completions|chat|paas/v\d+)$", re.I)


# ---------------------------------------------------------------- 小工具

def _num(value: Any) -> float:
    """把返回值读成数字；读不出来当 0。"""
    if value is None or isinstance(value, bool):
        return 0.0
    try:
        return float(value)
    except (TypeError, ValueError):
        return 0.0


def describe(exc: BaseException) -> str:
    """把异常压成一句能看懂的话（带原因码）。"""
    cause = getattr(getattr(exc, "reason", None), "strerror", "") or ""
    msg = str(exc) or type(exc).__name__
    return f"{msg}（{cause}）" if cause and cause not in msg else msg


def site_root(base_url: str) -> str:
    """从 base_url 推出站点根地址（签到接口挂在根上，不在 /v1 下面）。"""
    raw = str(base_url or "").strip()
    if not raw:
        return ""
    if not re.match(r"^https?://", raw, re.I):
        raw = "https://" + raw
    try:
        parts = urllib.parse.urlsplit(raw)
    except ValueError:
        return raw.rstrip("/")
    if not parts.scheme or not parts.netloc:
        return raw.rstrip("/")
    segs = [s for s in parts.path.split("/") if s]
    while segs and _TAIL_PREFIX_RE.match(segs[-1]):
        segs.pop()
    path = ("/" + "/".join(segs)) if segs else ""
    return urllib.parse.urlunsplit(
        (parts.scheme, parts.netloc, path, "", "")).rstrip("/")


# ---------------------------------------------------------------- HTTP

def _request(url: str, token: str, user_id: str = "",
             method: str = "GET", timeout: float = DEFAULT_TIMEOUT,
             proxy: str = "") -> tuple[int, dict, str]:
    """发一个请求，返回 (状态码, 响应头, 正文)。HTTP 错误码不抛异常。

    proxy = 这个上游的出口，跟推理请求走同一条路（http 和 socks5 都支持）。
    这样「这个站点走代理才通 / 走代理反而被人机验证拦」的差别不会让签到
    和推理打架。
    """
    headers = {
        "Accept": "application/json",
        "Accept-Encoding": "identity",
        "User-Agent": USER_AGENT,
        "Authorization": f"Bearer {token}",
    }
    if user_id:
        # 老版本 New API 认这个头；新版本带不带都一样
        headers["New-Api-User"] = str(user_id)
    proxy = str(proxy or "").strip()
    if proxy:
        return _request_via_proxy(url, method, headers, timeout, proxy)

    req = urllib.request.Request(url, method=method, headers=headers)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            return int(resp.status), dict(resp.headers or {}), \
                resp.read(262144).decode("utf-8", "replace")
    except urllib.error.HTTPError as exc:
        body = ""
        try:
            body = exc.read(262144).decode("utf-8", "replace")
        except Exception:  # noqa: BLE001 — 读不到正文就算了
            body = ""
        return int(exc.code), dict(exc.headers or {}), body


def _request_via_proxy(url: str, method: str, headers: dict, timeout: float,
                       proxy: str) -> tuple[int, dict, str]:
    """经代理（http CONNECT 或 socks5）发一个请求。

    借 router.py 里那套已经完全一样的连接代码 —— 签到和推理走同一个出口、
    同一套隧道实现，不会出现「推理能通、签到连不上」的假故障。
    """
    import router as router_mod          # 延迟导入：router 在模块级 import 了本文件
    spec = router_mod.ProxySpec.parse(proxy)
    if spec is None:
        raise ValueError(f"代理地址看不懂: {proxy!r}")
    parts = urllib.parse.urlsplit(url)
    scheme = (parts.scheme or "https").lower()
    if scheme not in ("http", "https") or not parts.hostname:
        raise ValueError(f"签到地址不合法: {url!r}")
    port = parts.port or (443 if scheme == "https" else 80)
    target = parts.path or "/"
    if parts.query:
        target += "?" + parts.query
    conn = router_mod.make_upstream_connection(scheme, parts.hostname, port,
                                              timeout, spec)
    try:
        conn.request(method, target, headers=headers)
        resp = conn.getresponse()
        body = resp.read(262144).decode("utf-8", "replace")
        return int(resp.status), dict(resp.getheaders() or {}), body
    finally:
        try:
            conn.close()
        except Exception:  # noqa: BLE001
            pass


def challenge_kind(status: int, headers: dict, text: str) -> str:
    """这个响应是不是 Cloudflare / Turnstile 的人机验证。返回 "" / "cf" / "turnstile"。

    只做识别，绝不尝试绕过（不解验证码、不伪造 cf_clearance）。
    """
    low_headers = {str(k).lower(): str(v) for k, v in (headers or {}).items()}
    if low_headers.get("cf-mitigated", "").lower() == "challenge":
        return "cf"

    is_json = False
    try:
        json.loads(text)
        is_json = True
    except Exception:  # noqa: BLE001
        is_json = False

    ctype = low_headers.get("content-type", "").lower()
    low = (text or "").lower()

    # 明确的 Turnstile 拒绝：New API 用 HTTP 200 + success:false + 带 turnstile 的 message
    if is_json and "turnstile" in low:
        return "turnstile"

    # 拦截页：HTML 或者根本不是 JSON
    if "html" in ctype or not is_json:
        for marker in ("just a moment", "__cf_chl_", "cdn-cgi/challenge-platform",
                       "attention required", "checking your browser"):
            if marker in low:
                return "cf"

    if status == 403 and "cloudflare" in low_headers.get("server", "").lower():
        return "cf"
    return ""


def _json_or_none(text: str) -> Optional[dict]:
    """正文能解析成 JSON 对象就返回它，否则 None（多半是兜底的网页）。"""
    if not text:
        return None
    try:
        obj = json.loads(text)
    except Exception:  # noqa: BLE001
        return None
    return obj if isinstance(obj, dict) else None


# ---------------------------------------------------------------- 结果解析

def _parse_state(payload: dict) -> dict:
    """把 GET 签到状态的信封读成统一结构。"""
    data = payload.get("data")
    if not isinstance(data, dict):
        data = payload if isinstance(payload, dict) else {}
    stats = data.get("stats") if isinstance(data.get("stats"), dict) else {}
    records = stats.get("records")
    if not isinstance(records, list):
        records = data.get("records") if isinstance(data.get("records"), list) else []
    return {
        # 站点是否开启签到功能
        "enabled": data.get("enabled") is True,
        "min_quota": _num(data.get("min_quota")),
        "max_quota": _num(data.get("max_quota")),
        "checked_in_today": stats.get("checked_in_today") is True,
        "total_checkins": int(_num(stats.get("total_checkins") or stats.get("checkin_count"))),
        "total_quota": _num(stats.get("total_quota")),
        "records": records[:30],
    }


def _parse_press(payload: dict) -> tuple[bool, str, float, str]:
    """把 POST 签到的信封读成 (成功?, 说明, 奖励额度, 签到日期)。"""
    data = payload.get("data")
    if not isinstance(data, dict):
        data = payload if isinstance(payload, dict) else {}
    success = (
        payload.get("success") is True
        or data.get("success") is True
        or str(payload.get("message") or "") == "success"
    )
    message = str(payload.get("message") or data.get("message") or "").strip()
    reward = _num(data.get("quota_awarded") if data.get("quota_awarded") is not None
                  else data.get("reward"))
    day = str(data.get("checkin_date") or data.get("date") or "")
    return success, message, reward, day


# ---------------------------------------------------------------- 额度单位

def fetch_quota_per_unit(root: str, timeout: float = 8.0,
                         proxy: str = "") -> Optional[dict]:
    """读 /api/status 的额度换算。失败返回 None（不影响签到本身）。"""
    try:
        status, _headers, text = _request(f"{root}/api/status", "",
                                          timeout=timeout, proxy=proxy)
    except Exception:  # noqa: BLE001
        return None
    if status >= 400:
        return None
    payload = _json_or_none(text)
    if not payload:
        return None
    data = payload.get("data") if isinstance(payload.get("data"), dict) else payload
    per_unit = _num(data.get("quota_per_unit"))
    if per_unit <= 0:
        per_unit = 500000.0
    return {
        "quota_per_unit": per_unit,
        "currency": data.get("display_in_currency") is not False,
        "symbol": str(data.get("custom_currency_symbol") or ""),
        "checkin_enabled": data.get("checkin_enabled") is True,
        "turnstile": data.get("turnstile_check") is True or data.get("turnstile") is True,
    }


def _fmt_reward(reward: float, per_unit: Optional[float], symbol: str = "") -> tuple[float, str]:
    """额度原始单位换成金额；换不了就返回 0 和空串。"""
    if reward <= 0 or not per_unit:
        return 0.0, ""
    usd = reward / per_unit
    return round(usd, 6), f"{symbol}{usd:.4f}".strip() if symbol else f"${usd:.4f}"


# ---------------------------------------------------------------- 探测 / 签到

def _config_of(item: Any) -> dict:
    """兼容 dict 与 Upstream 对象两种入参。"""
    if isinstance(item, dict):
        return item
    out = {"base_url": getattr(item, "base_url", ""), "name": getattr(item, "name", ""),
           "id": getattr(item, "id", ""), "checkin": {},
           # 出口（Upstream 对象直接进来时也带上，dict 进来的话用上面那个键）
           "proxy_url": str(getattr(item, "proxy_url", "") or "")}
    ck = {}
    token = getattr(item, "checkin_token", "")
    if token:
        ck = {"token": token,
              "user_id": getattr(item, "checkin_user_id", ""),
              "enabled": getattr(item, "checkin_enabled", True),
              "path": getattr(item, "checkin_path", ""),
              "quota_per_unit": getattr(item, "checkin_quota_per_unit", 0)}
    out["checkin"] = ck
    return out


def _candidates(ck: dict) -> list[str]:
    """先问上次学到的路径，再问两个候选。"""
    known = str(ck.get("path") or "").strip()
    order = [p for p in CHECKIN_PATHS if p != known]
    return ([known] if known in CHECKIN_PATHS else []) + order


def _result(kind: str, ok: bool, message: str, **extra) -> dict:
    out = {
        "ok": ok,
        "kind": kind,
        "at": time.time(),
        "day": time.strftime("%Y-%m-%d"),
        "message": message,
    }
    out.update(extra)
    return out


def _walk(root: str, ck: dict, press_checkin: bool,
          timeout: float, proxy: str = "") -> dict:
    """依次试候选路径，返回第一条能用的路径的结果。"""
    token = str(ck.get("token") or "").strip()
    user_id = str(ck.get("user_id") or "").strip()
    last_error = _result("none", False, "站点没有可用的签到接口")
    saw_auth = False

    for path in _candidates(ck):
        url = root + path
        try:
            status, headers, text = _request(
                url, token, user_id,
                method="POST" if press_checkin else "GET", timeout=timeout,
                proxy=proxy)
        except (urllib.error.URLError, socket.timeout, TimeoutError, OSError) as exc:
            last_error = _result("error", False, f"连不上 {root}：{describe(exc)}")
            continue

        kind = challenge_kind(status, headers, text)
        if kind:
            return _result(
                "blocked", False,
                "站点要求人机验证（Cloudflare/Turnstile），访问令牌过不去，"
                "请在站点上用浏览器手动签到",
                blocked=kind, status=status)

        if status in (401, 403):
            saw_auth = True
            last_error = _result("auth", False, f"站点拒绝了访问令牌（HTTP {status}）",
                                 status=status)
            continue

        # 404/405 = 这个版本没有这个路径，换下一个候选
        if status in (404, 405):
            last_error = _result("none", False, f"站点没有 {path}（HTTP {status}）",
                                 status=status)
            continue

        payload = _json_or_none(text)
        if payload is None:
            # 200 + 网页：网关把首页兜底返回了，这个地址不是签到接口
            last_error = _result("none", False, f"{path} 返回的不是 JSON（多半是首页）",
                                 status=status)
            continue

        if status >= 400:
            msg = str(payload.get("message") or "")[:200]
            last_error = _result("error", False,
                                 f"HTTP {status}：{msg or '站点拒绝请求'}", status=status)
            continue

        if not press_checkin:
            state = _parse_state(payload)
            if payload.get("data") is None and payload.get("success") is None:
                last_error = _result("none", False, f"{path} 答的不是签到状态",
                                     status=status)
                continue
            if state["enabled"] is False:
                return _result("none", False, "站点没开启签到功能",
                               path=path, state=state, status=status)
            done = state["checked_in_today"]
            return _result(
                "already" if done else "ok", True,
                "今天已经签过了" if done else "可以签到",
                path=path, state=state, status=status,
                checked_in_today=done,
                total_checkins=state["total_checkins"],
                total_quota=state["total_quota"])

        # ---- 真的按下去 ----
        success, message, reward, day = _parse_press(payload)
        if success:
            return _result("ok", True, message or "签到成功",
                           path=path, reward=reward, checkin_date=day, status=status)
        low = (message or "").lower()
        if "already" in low or "已签到" in message or "重复" in message:
            return _result("already", True, message or "今天已经签过了",
                           path=path, checked_in_today=True, status=status)
        last_error = _result("error", False, message or "站点没有接受这次签到",
                             status=status, path=path)

    if saw_auth and last_error.get("kind") == "none":
        last_error = _result("auth", False, "站点拒绝了访问令牌")
    return last_error


def probe(item: Any, timeout: float = DEFAULT_TIMEOUT) -> dict:
    """只读探测：这个上游能不能签到、今天签过没有。不发 POST。"""
    return run_one(item, press_checkin=False, timeout=timeout)


def press(item: Any, timeout: float = DEFAULT_TIMEOUT) -> dict:
    """执行签到（会先读状态，今天签过就不再按）。"""
    return run_one(item, press_checkin=True, timeout=timeout)


def run_one(item: Any, press_checkin: bool = True,
            timeout: float = DEFAULT_TIMEOUT) -> dict:
    """对一个上游跑一遍签到。失败绝不抛异常，结果里带 kind/message。"""
    cfg = _config_of(item)
    name = str(cfg.get("name") or cfg.get("id") or "?")
    ck = cfg.get("checkin") or {}
    token = str(ck.get("token") or "").strip()
    # 出口跟推理请求一条路（router 那边算好传进来，这里只负责用）
    proxy = str(cfg.get("proxy_url") or "").strip()

    if not token:
        return _result("nocfg", False, "还没配置访问令牌")

    # 访问令牌是登录令牌，不是 sk- 推理 key：填反了签到永远过不去，直接点出来
    if token.lower().startswith("sk-"):
        return _result("nocfg", False,
                       "这里要填站点的「访问令牌」，不是 sk- 开头的推理 Key")

    root = site_root(cfg.get("base_url") or "")
    if not root:
        return _result("error", False, "上游没填地址")

    base_result = _result("none", False, "未知")
    try:
        if press_checkin:
            # 先读状态：今天签过就不必再按，也顺便验证路径。
            # 读状态这一步顺带把「根本不该按」的几种情况挡掉：
            # 没有签到接口、站点关着签到、令牌不行、连不上 —— 都不发 POST。
            state_result = _walk(root, ck, False, timeout, proxy)
            state_kind = state_result.get("kind")
            if state_kind == "blocked":
                return state_result
            if state_result.get("checked_in_today"):
                return _result(
                    "already", True, "今天已经签过了",
                    path=state_result.get("path", ""),
                    checked_in_today=True,
                    total_checkins=state_result.get("total_checkins", 0),
                    total_quota=state_result.get("total_quota", 0))
            if state_kind != "ok":
                # nocfg / auth / none（没接口或站点没开签到）/ error
                return state_result
            # 走到这里：接口在、今天还没签。顺着刚学到的路径按下去
            if state_result.get("path"):
                ck = {**ck, "path": state_result["path"]}
            base_result = _walk(root, ck, True, timeout, proxy)
        else:
            base_result = _walk(root, ck, False, timeout, proxy)
    except Exception as exc:  # noqa: BLE001 — 签到线程绝不能因为一个站点崩掉
        return _result("error", False, f"签到出错：{describe(exc)}")

    # 额度换算：只成功时才问，问不到就退回原始额度
    per_unit = _num(ck.get("quota_per_unit")) or None
    symbol = ""
    if base_result.get("ok") and per_unit is None:
        status_info = fetch_quota_per_unit(root, timeout=min(timeout, 8.0),
                                           proxy=proxy)
        if status_info:
            per_unit = status_info.get("quota_per_unit")
            symbol = status_info.get("symbol", "") if status_info.get("currency") else ""
            base_result["quota_per_unit"] = per_unit
    elif per_unit:
        base_result.setdefault("quota_per_unit", per_unit)

    reward = _num(base_result.get("reward"))
    if reward > 0:
        usd, pretty = _fmt_reward(reward, per_unit, symbol)
        base_result["reward_usd"] = usd
        base_result["reward_text"] = pretty

    base_result["name"] = name
    base_result["id"] = str(cfg.get("id") or "")
    return base_result


# ---------------------------------------------------------------- 统计

class CheckinStats:
    """签到明细，落在 .llmrouter/<配置名>.checkin.json。

    只存「真的按下去过」的结果（ok / already），失败也记一条，
    这样签到页能回答「昨天为什么没签上」。
    """

    def __init__(self, dir_path: str, stem: str, keep: int = STATS_KEEP):
        self.dir_path = dir_path
        self.stem = stem
        self.keep = max(100, int(keep))
        self._lock = threading.Lock()
        self.entries: list[dict] = []
        self.load()

    @property
    def path(self) -> str:
        return os.path.join(self.dir_path, f"{self.stem}.checkin.json")

    def load(self) -> None:
        try:
            with open(self.path, "r", encoding="utf-8") as fh:
                data = json.load(fh)
        except Exception:  # noqa: BLE001 — 没有或读坏了都当空
            self.entries = []
            return
        raw = data.get("entries") if isinstance(data, dict) else data
        self.entries = [e for e in raw if isinstance(e, dict)][-self.keep:] \
            if isinstance(raw, list) else []

    def append(self, entry: dict) -> None:
        with self._lock:
            self.entries.append(entry)
            if len(self.entries) > self.keep:
                self.entries = self.entries[-self.keep:]
            self._save_locked()

    def _save_locked(self) -> None:
        try:
            os.makedirs(self.dir_path, exist_ok=True)
            tmp = self.path + ".tmp"
            with open(tmp, "w", encoding="utf-8") as fh:
                json.dump({"entries": self.entries}, fh,
                          ensure_ascii=False, indent=1)
            os.replace(tmp, self.path)
        except Exception:  # noqa: BLE001 — 统计写不进去不该影响签到
            pass

    def recent(self, limit: int = 200) -> list[dict]:
        with self._lock:
            return list(self.entries[-max(1, limit):])[::-1]

    def summary(self) -> dict:
        """按上游汇总：累计成功次数、总奖励、最近一次。"""
        with self._lock:
            entries = list(self.entries)
        per: dict[str, dict] = {}
        for e in entries:
            uid = str(e.get("id") or "")
            if not uid:
                continue
            row = per.setdefault(uid, {
                "id": uid, "name": e.get("name", ""),
                "runs": 0, "ok": 0, "already": 0, "blocked": 0, "failed": 0,
                "reward": 0.0, "reward_usd": 0.0, "last_at": 0.0,
                "last_kind": "", "last_message": "",
            })
            row["runs"] += 1
            kind = str(e.get("kind") or "")
            if kind == "ok":
                row["ok"] += 1
            elif kind == "already":
                row["already"] += 1
            elif kind == "blocked":
                row["blocked"] += 1
            else:
                row["failed"] += 1
            row["reward"] += _num(e.get("reward"))
            row["reward_usd"] += _num(e.get("reward_usd"))
            at = _num(e.get("at"))
            if at >= row["last_at"]:
                row["last_at"] = at
                row["last_kind"] = kind
                row["last_message"] = str(e.get("message") or "")[:200]
        today = time.strftime("%Y-%m-%d")
        today_ok = sum(1 for e in entries
                       if e.get("day") == today and e.get("kind") in ("ok", "already"))
        return {
            "total": len(entries),
            "today_checked": today_ok,
            "today": today,
            "per_upstream": sorted(per.values(),
                                   key=lambda r: (-r["last_at"], r["name"])),
        }


# ---------------------------------------------------------------- 自动签到

def _parse_hhmm(text: str, fallback: tuple[int, int] = (8, 0)) -> tuple[int, int]:
    m = re.match(r"^\s*(\d{1,2})\s*[:：]\s*(\d{1,2})\s*$", str(text or ""))
    if not m:
        return fallback
    hh, mm = int(m.group(1)), int(m.group(2))
    if 0 <= hh <= 23 and 0 <= mm <= 59:
        return hh, mm
    return fallback


def parse_hhmm(text: Any, fallback: tuple[int, int] = (8, 0)) -> tuple[int, int]:
    """"08:00" / "8：30" → (8, 0) / (8, 30)；认不出来就用 fallback。"""
    return _parse_hhmm(text, fallback)


class CheckinScheduler:
    """每天在「设定时刻 + 随机窗口」跑一次自动签到。

    用随机窗口是因为所有上游都用同一个固定时刻打过去太假，
    站点也容易把你和定时脚本对上号。窗口内随机取一个时刻，当天固定。

    服务是中转重启的常客：启动时如果今天的时刻已经过了、而今天还没签过，
    就补一次（用小延迟，避免启动瞬间就并发打几十个站点）。
    """

    def __init__(self, router, tick: float = 60.0):
        self.router = router
        self.tick = max(15.0, float(tick))
        self._stop = threading.Event()
        self._thread: Optional[threading.Thread] = None
        self._plan_day = ""      # 计划属于哪一天
        self._plan_ts = 0.0      # 当天随机出来的执行时刻
        self._last_day = ""      # 最近一次真的跑过的日期

    def start(self) -> None:
        if self._thread is not None:
            return
        self._thread = threading.Thread(target=self._loop, name="checkin-scheduler",
                                        daemon=True)
        self._thread.start()

    def stop(self) -> None:
        self._stop.set()

    def reset_plan(self) -> None:
        """用户改了签到时刻：把今天的随机计划作废，下一拍按新时刻重算。

        _last_day 特意不清：今天已经签过就是签过了，改个时间不该再签一遍。
        """
        self._plan_day = ""
        self._plan_ts = 0.0

    def _settings(self) -> dict:
        try:
            return self.router.checkin_settings()
        except Exception:  # noqa: BLE001
            return {"enabled": False}

    def _plan_for(self, day: str, hh: int, mm: int, window_min: int) -> float:
        """当天要执行的时刻：基准时刻 + [0, window] 里的随机秒数。"""
        if self._plan_day == day:
            return self._plan_ts
        now = time.localtime()
        base = time.mktime((now.tm_year, now.tm_mon, now.tm_mday,
                            hh, mm, 0, 0, 0, -1))
        offset = random.randint(0, max(0, int(window_min)) * 60)
        self._plan_day = day
        self._plan_ts = base + offset
        return self._plan_ts

    def _loop(self) -> None:
        # 启动先等一拍：别和「拉模型」「镜像探测」抢连接
        if self._stop.wait(20.0):
            return
        while not self._stop.is_set():
            try:
                self._tick()
            except Exception:  # noqa: BLE001 — 调度线程绝不能死
                pass
            if self._stop.wait(self.tick):
                break

    def _tick(self) -> None:
        cfg = self._settings()
        if not cfg.get("enabled"):
            return
        hh, mm = _parse_hhmm(cfg.get("at"), (8, 0))
        window = int(_num(cfg.get("random_minutes")))
        day = time.strftime("%Y-%m-%d")
        if self._last_day == day:
            return                                  # 今天已经自动跑过
        target = self._plan_for(day, hh, mm, window)
        if time.time() < target:
            return                                  # 还没到点
        if not any(getattr(u, "checkin_enabled", False)
                   and getattr(u, "checkin_token", "")
                   for u in getattr(self.router, "upstreams", [])):
            return                                  # 一个都没配令牌，别空转
        try:
            self.router.checkin_all(reason="auto")
        finally:
            self._last_day = day
            self._plan_day = ""                     # 明天重新抽一次
