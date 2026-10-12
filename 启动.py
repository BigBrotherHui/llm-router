#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""一键启动：优先 config.toml，没有就用 config.example.toml 跑起来给你看看。"""
import os
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROUTER = os.path.join(HERE, "router.py")

cfg = os.path.join(HERE, "config.toml")
if not os.path.exists(cfg):
    example = os.path.join(HERE, "config.example.toml")
    print("提示：还没看到 config.toml。")
    print("     先复制一份：copy config.example.toml config.toml")
    print("     （下面先用示例配置做一次校验，不会真的启动服务）\n")
    sys.exit(subprocess.call([sys.executable, ROUTER, "-c", example, "--check"]))

print("启动后请打开网页控制台： http://127.0.0.1:8010/\n")
sys.exit(subprocess.call([sys.executable, ROUTER, "-c", cfg]))
