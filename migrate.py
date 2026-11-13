#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""llm-router 迁移工具：把「这台机器上的全部上游」导出成一个文件，换机器一条命令导入。

为什么需要它
    上游（名字 / 地址 / Key / 勾选 / 分组 / 模型路由）全在
    .llmrouter/<配置名>.state.json 里，而这个目录 .gitignore 掉了 ——
    换台电脑 clone 下来是空的，得一个个重新填。这个脚本就是补这一步。

用法
    python migrate.py export                 # 导出到 ./迁移包/
    python migrate.py export --no-secrets    # 导出时把 Key 打码（给别人看/存档用）
    python migrate.py list  <包>             # 只看包里有哪些上游，不落盘
    python migrate.py import <包>            # 还原 config.toml + 全部上游/路由

导出的东西
    迁移包/
      bundle.json          全量（config.toml 原文 + state + 签到记录），import 靠它
      upstreams.full.json  纯上游清单，Key 明文
      upstreams.toml       可直接粘进 config.toml 的 [[upstreams]] 片段
      upstreams.masked.md  脱敏清单（Key 只剩头尾），贴文档/发给别人用这个
    llm-router-migrate-<时间戳>.zip   上面四个打成一个包

包里含明文 Key，别传公共仓库 —— .gitignore 已经把「迁移包/」和 zip 排除掉了。
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import sys
import time
import zipfile

SCHEMA = "llm-router-migrate/1"
DEFAULT_CONFIG = "config.toml"
STATE_DIRNAME = ".llmrouter"
EXPORT_DIRNAME = "迁移包"


# ---------------------------------------------------------------- 小工具

def _state_stem(config_path: str) -> str:
    return os.path.splitext(os.path.basename(config_path))[0]


def _read_text(path: str) -> str:
    with open(path, "r", encoding="utf-8") as fh:
        return fh.read()


def _write_text(path: str, text: str) -> None:
    os.makedirs(os.path.dirname(os.path.abspath(path)) or ".", exist_ok=True)
    with open(path, "w", encoding="utf-8", newline="\n") as fh:
        fh.write(text)


def _read_json(path: str):
    with open(path, "r", encoding="utf-8") as fh:
        return json.load(fh)


def _write_json(path: str, data) -> None:
    _write_text(path, json.dumps(data, ensure_ascii=False, indent=1) + "\n")


def _mask(key: str) -> str:
    if not key:
        return "（空）"
    if len(key) <= 12:
        return key[:3] + "*" * (len(key) - 3)
    return f"{key[:6]}…{key[-4:]}（{len(key)} 位）"


def _stamp() -> str:
    return time.strftime("%Y%m%d-%H%M%S")


def _backup(path: str, suffix: str) -> str:
    """覆盖前留一份，撞名就加序号。返回备份路径（文件不存在时返回空串）。"""
    if not os.path.exists(path):
        return ""
    target = f"{path}.bak-{suffix}"
    seq = 1
    while os.path.exists(target):
        target = f"{path}.bak-{suffix}-{seq}"
        seq += 1
    shutil.copyfile(path, target)
    return target


# ---------------------------------------------------------------- 归一化

def _upstream_rows(state: dict) -> list[dict]:
    """把 state 里的上游压成一份干净、可读、可重建的清单。"""
    rows = []
    for up in state.get("upstreams") or []:
        row = {
            "id": up.get("id") or "",
            "name": up.get("name") or "",
            "base_url": up.get("base_url") or "",
            "base_urls": list(up.get("base_urls") or []),
            "mirror_strategy": up.get("mirror_strategy") or "",
            "api_keys": list(up.get("api_keys") or []),
            "api_key_names": list(up.get("api_key_names") or []),
            "key_strategy": up.get("key_strategy") or "",
            "enabled": bool(up.get("enabled", True)),
            "selected_models": list(up.get("selected_models") or []),
            "model_groups": dict(up.get("model_groups") or {}),
            "available_models": list(up.get("available_models") or []),
            "first_timeout_seconds": up.get("first_timeout_seconds"),
            "user_agent": up.get("user_agent") or "",
            "checkin": up.get("checkin") or {},
        }
        rows.append(row)
    return rows


def _toml_quote(text: str) -> str:
    return '"' + str(text).replace("\\", "\\\\").replace('"', '\\"') + '"'


def _upstreams_toml(rows: list[dict]) -> str:
    """生成可直接粘进 config.toml 的 [[upstreams]] 片段。

    config.toml 一个上游只认一个 api_key，多 Key 的只在注释里点名，
    真正多 Key 的完整信息在 bundle.json / state.json 里 —— 用 import 恢复才不丢。
    """
    out = [
        "# ============================================================",
        "# 由 migrate.py 导出，可直接粘到 config.toml 末尾。",
        "# 注意：config.toml 一个上游只写一个 api_key；多 Key 的完整写法",
        "#      在 bundle.json 里，用 `python migrate.py import` 恢复不会丢。",
        "# ============================================================",
        "",
    ]
    for row in rows:
        out.append(f"# --- {row['name'] or row['id']} ---")
        out.append("[[upstreams]]")
        out.append(f"name = {_toml_quote(row['name'])}")
        out.append(f"id = {_toml_quote(row['id'])}")
        out.append(f"base_url = {_toml_quote(row['base_url'])}")
        if row["base_urls"]:
            urls = ", ".join(_toml_quote(u) for u in row["base_urls"])
            out.append(f"base_urls = [{urls}]")
            if row["mirror_strategy"]:
                out.append(f"mirror_strategy = {_toml_quote(row['mirror_strategy'])}")
        keys = row["api_keys"]
        out.append(f"api_key = {_toml_quote(keys[0] if keys else '')}")
        if len(keys) > 1:
            out.append(f"# 这个上游有 {len(keys)} 个 Key，其余 {len(keys) - 1} 个只在 state 里：")
            out.append("# " + " / ".join(_mask(k) for k in keys[1:]))
        if not row["enabled"]:
            out.append("# 注意：导出时这个上游是停用状态（enabled = false）")
        out.append("")
    return "\n".join(out)


def _masked_markdown(rows: list[dict], state: dict, meta: dict) -> str:
    lines = [
        "# 上游清单（脱敏）",
        "",
        f"- 导出时间：{meta['exported_at']}",
        f"- 来源机器：{meta['host']}",
        f"- 上游 {len(rows)} 个（启用 {sum(1 for r in rows if r['enabled'])} 个），"
        f"Key {sum(len(r['api_keys']) for r in rows)} 个，"
        f"模型路由 {len(state.get('models') or {})} 条",
        f"- Key 已打码，只用于核对；明文在 bundle.json / upstreams.full.json 里。",
        "",
        "| # | 名称 | id | base_url | 线路 | Key | 状态 | 勾选模型 |",
        "| --- | --- | --- | --- | --- | --- | --- | --- |",
    ]
    for i, row in enumerate(rows, 1):
        keys = "<br>".join(f"`{_mask(k)}`" for k in row["api_keys"]) or "（无，本地服务）"
        urls = f"{row['base_url']}"
        if row["base_urls"]:
            urls += f"<br>+{len(row['base_urls']) - 1} 条镜像"
        lines.append(
            f"| {i} | {row['name']} | `{row['id']}` | {urls} | "
            f"{len(row['base_urls']) or 1} | {keys} | "
            f"{'✅ 启用' if row['enabled'] else '⏸ 停用'} | {len(row['selected_models'])} |"
        )
    lines.append("")
    lines.append("## 模型路由")
    lines.append("")
    lines.append("| 客户端模型名 | 策略 | 条目 |")
    lines.append("| --- | --- | --- |")
    for name, spec in (state.get("models") or {}).items():
        spec = spec or {}
        entries = spec.get("entries") or []
        first = entries[0] if entries else {}
        text = f"`{first.get('upstream', '')}` / {first.get('model', '')}"
        if len(entries) > 1:
            text += f" 等 {len(entries)} 个"
        lines.append(f"| `{name}` | {spec.get('strategy', '')} | {text} |")
    lines.append("")
    return "\n".join(lines)


# ---------------------------------------------------------------- export

def cmd_export(args) -> int:
    root = os.path.abspath(args.root)
    config_path = os.path.join(root, args.config)
    state_path = os.path.join(root, STATE_DIRNAME, f"{_state_stem(args.config)}.state.json")
    checkin_path = os.path.join(root, STATE_DIRNAME, f"{_state_stem(args.config)}.checkin.json")

    if not os.path.exists(state_path):
        print(f"找不到状态文件：{state_path}")
        print("  先在网页控制台里配好上游，或者确认 -c 的配置文件名对不对。")
        return 2
    state = _read_json(state_path)
    rows = _upstream_rows(state)

    if args.no_secrets:
        for row in rows:
            row["api_keys"] = [_mask(k) for k in row["api_keys"]]
        for up in state.get("upstreams") or []:
            if up.get("api_keys"):
                up["api_keys"] = [_mask(k) for k in up["api_keys"]]
            if up.get("checkin"):
                up["checkin"] = {}

    host = os.environ.get("COMPUTERNAME") or ""
    if not host:
        host = os.uname().nodename if hasattr(os, "uname") else ""
    meta = {
        "exported_at": time.strftime("%Y-%m-%d %H:%M:%S"),
        "host": host,
        "python": sys.version.split()[0],
        "config": args.config,
        "secrets": "masked" if args.no_secrets else "plain",
        "root": root,
    }

    out_dir = args.out or os.path.join(root, EXPORT_DIRNAME)
    os.makedirs(out_dir, exist_ok=True)

    bundle = {
        "schema": SCHEMA,
        "meta": meta,
        "summary": {
            "upstreams": len(rows),
            "enabled": sum(1 for r in rows if r["enabled"]),
            "keys": sum(len(r["api_keys"]) for r in rows),
            "models": len(state.get("models") or {}),
        },
        "config_toml": _read_text(config_path) if os.path.exists(config_path) else "",
        "state": state,
        "checkin_log": _read_json(checkin_path) if os.path.exists(checkin_path) else {},
        "upstreams": rows,
    }

    bundle_path = os.path.join(out_dir, "bundle.json")
    full_path = os.path.join(out_dir, "upstreams.full.json")
    toml_path = os.path.join(out_dir, "upstreams.toml")
    md_path = os.path.join(out_dir, "upstreams.masked.md")

    _write_json(bundle_path, bundle)
    _write_json(full_path, {"meta": meta, "summary": bundle["summary"], "upstreams": rows})
    _write_text(toml_path, _upstreams_toml(rows))
    _write_text(md_path, _masked_markdown(rows, state, meta))

    zip_path = os.path.join(root, f"llm-router-migrate-{_stamp()}.zip")
    with zipfile.ZipFile(zip_path, "w", zipfile.ZIP_DEFLATED) as zf:
        for path in (bundle_path, full_path, toml_path, md_path):
            zf.write(path, os.path.basename(path))

    print(f"上游 {len(rows)} 个 / 启用 {bundle['summary']['enabled']} 个 / "
          f"Key {bundle['summary']['keys']} 个 / 模型路由 {bundle['summary']['models']} 条")
    print()
    for path in (bundle_path, full_path, toml_path, md_path):
        print(f"  {path}  ({os.path.getsize(path)} 字节)")
    print(f"  {zip_path}  ({os.path.getsize(zip_path)} 字节)")
    if args.no_secrets:
        print("\n注意：这次导出的 Key 是打码的，只能看不能用，重新导入也恢复不出真 Key。")
    else:
        print("\n包里是明文 Key：别传到公开仓库，只走 U 盘 / 私有网盘 / scp。")
    return 0


# ---------------------------------------------------------------- import

def _load_bundle(path: str) -> dict:
    if os.path.isdir(path):
        path = os.path.join(path, "bundle.json")
    if zipfile.is_zipfile(path):
        tmp = os.path.join(os.path.dirname(os.path.abspath(path)), f"._unzip_{_stamp()}")
        os.makedirs(tmp, exist_ok=True)
        with zipfile.ZipFile(path) as zf:
            names = zf.namelist()
            if "bundle.json" not in names:
                raise SystemExit(f"包里没有 bundle.json：{names}")
            zf.extract("bundle.json", tmp)
        try:
            return _read_json(os.path.join(tmp, "bundle.json"))
        finally:
            shutil.rmtree(tmp, ignore_errors=True)
    data = _read_json(path)
    if data.get("schema") != SCHEMA:
        raise SystemExit(f"不认的包格式：schema={data.get('schema')!r}（应为 {SCHEMA}）")
    return data


def cmd_list(args) -> int:
    bundle = _load_bundle(args.bundle)
    meta = bundle.get("meta") or {}
    rows = bundle.get("upstreams") or _upstream_rows(bundle.get("state") or {})
    print(f"导出时间 {meta.get('exported_at')}  来源 {meta.get('host')}  "
          f"Key 状态 {meta.get('secrets')}")
    print(f"上游 {len(rows)} 个，模型路由 {len((bundle.get('state') or {}).get('models') or {})} 条")
    print()
    for i, row in enumerate(rows, 1):
        keys = ", ".join(_mask(k) for k in row["api_keys"]) or "（无 Key）"
        flag = "" if row["enabled"] else "  [停用]"
        print(f"{i:>2}. {row['name']:<28} {row['base_url']:<45} {keys}{flag}")
    return 0


def cmd_import(args) -> int:
    root = os.path.abspath(args.root)
    bundle = _load_bundle(args.bundle)
    state = bundle.get("state") or {}
    rows = bundle.get("upstreams") or _upstream_rows(state)
    config_name = args.config or (bundle.get("meta") or {}).get("config") or DEFAULT_CONFIG
    stem = _state_stem(config_name)

    if (bundle.get("meta") or {}).get("secrets") == "masked" and not args.allow_masked:
        print("这个包里的 Key 是打码的（export --no-secrets 导出的），导进去也用不了。")
        print("  确认要导就加 --allow-masked；否则回原机器重新 export 一份。")
        return 3

    config_path = os.path.join(root, config_name)
    state_path = os.path.join(root, STATE_DIRNAME, f"{stem}.state.json")
    checkin_path = os.path.join(root, STATE_DIRNAME, f"{stem}.checkin.json")
    stamp = _stamp()

    plan = []
    if not args.dry_run:
        os.makedirs(os.path.join(root, STATE_DIRNAME), exist_ok=True)

    # 1) config.toml：已有就备份，包里没有就跳过
    text = bundle.get("config_toml") or ""
    if text:
        backup = _backup(config_path, f"import-{stamp}") if not args.dry_run else ""
        if not args.dry_run:
            _write_text(config_path, text)
        plan.append(f"config.toml  {'已写入' if not args.dry_run else '将写入'}"
                    f"{f'（原文件备份到 {os.path.basename(backup)}）' if backup else ''}")
    else:
        plan.append("config.toml  包里没有内容，跳过（沿用机器上现有的）")

    # 2) state：上游 / 勾选 / 分组 / 模型路由
    if args.merge:
        old = _read_json(state_path) if os.path.exists(state_path) else {}
        merged = _merge_state(old, state)
        state = merged
        plan.append("state  已和本机现有配置合并（按 id / 名称+地址去重，本机模型路由优先保留）")
    if not args.dry_run:
        _backup(state_path, f"import-{stamp}")
        _write_json(state_path, state)

    # 3) 签到日志：只做记录用，丢了无所谓
    checkin_log = bundle.get("checkin_log") or {}
    if checkin_log and not args.dry_run:
        _backup(checkin_path, f"import-{stamp}")
        _write_json(checkin_path, checkin_log)

    enabled = sum(1 for r in rows if r["enabled"])
    keys = sum(len(r["api_keys"]) for r in rows)
    print(("预览（--dry-run，没动文件）：" if args.dry_run else "导入完成："))
    for line in plan:
        print("  " + line)
    print(f"  上游 {len(rows)} 个（启用 {enabled}）/ Key {keys} 个 / "
          f"模型路由 {len(state.get('models') or {})} 条")
    print(f"  状态文件 {state_path}")
    print()
    print("下一步：启动 llm-router 桌面版（desktop/dist/llm-router-native.exe 或安装包），")
    return 0


def _merge_state(old: dict, new: dict) -> dict:
    """把导入的 state 合进本机 state：上游按 id / (名称+地址) 去重，本机路由不被覆盖。"""
    merged = dict(old or {})
    merged.setdefault("version", new.get("version", 2))

    old_ups = list((old or {}).get("upstreams") or [])
    by_id = {u.get("id"): i for i, u in enumerate(old_ups)}
    by_name = {(u.get("name"), u.get("base_url")): i for i, u in enumerate(old_ups)}

    for up in new.get("upstreams") or []:
        idx = by_id.get(up.get("id"))
        if idx is None:
            idx = by_name.get((up.get("name"), up.get("base_url")))
        if idx is None:
            old_ups.append(up)
        else:
            keep = dict(up)
            keep.update({k: v for k, v in old_ups[idx].items() if v not in (None, "", [], {})})
            old_ups[idx] = keep
    merged["upstreams"] = old_ups

    models = dict(new.get("models") or {})
    models.update(old.get("models") or {})
    merged["models"] = models
    for key in ("suppressed_models", "defaults", "checkin_settings"):
        if key in old:
            merged[key] = old[key]
        elif key in new:
            merged[key] = new[key]
    return merged


# ---------------------------------------------------------------- CLI

def main(argv=None) -> int:
    ap = argparse.ArgumentParser(
        prog="migrate.py",
        description="导出/导入 llm-router 的全部上游（含 Key、勾选、分组、模型路由）")
    # --root / -c 挂在每个子命令上（argparse 允许放到子命令后面，写成
    # `import <包> --root D:\proj` 比 `--root D:\proj import <包>` 顺手）
    common = argparse.ArgumentParser(add_help=False)
    common.add_argument("--root", default=".", help="项目根目录（默认当前目录）")
    common.add_argument("-c", "--config", default=None,
                        help="配置文件名（默认 config.toml）")

    sub = ap.add_subparsers(dest="action", required=True)

    ex = sub.add_parser("export", parents=[common],
                        help="导出全部上游到 迁移包/ 并打 zip")
    ex.add_argument("-o", "--out", default=None, help="输出目录（默认 ./迁移包）")
    ex.add_argument("--no-secrets", action="store_true", help="Key 打码，只用于核对/分享")
    ex.set_defaults(func=cmd_export)

    li = sub.add_parser("list", parents=[common], help="看包里有哪些上游（不落盘）")
    li.add_argument("bundle", help="bundle.json / zip / 解压后的目录")
    li.set_defaults(func=cmd_list)

    im = sub.add_parser("import", parents=[common],
                        help="把包还原成本机的 config.toml + 状态")
    im.add_argument("bundle", help="bundle.json / zip / 解压后的目录")
    im.add_argument("-y", "--yes", action="store_true", help="不再问（脚本里用）")
    im.add_argument("--merge", action="store_true",
                    help="和本机现有配置合并，而不是整份覆盖")
    im.add_argument("--dry-run", action="store_true", help="只打印将要做什么")
    im.add_argument("--allow-masked", action="store_true", help="允许导入打码过的包")
    im.set_defaults(func=cmd_import)

    args = ap.parse_args(argv)
    if args.config is None:
        args.config = DEFAULT_CONFIG
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())