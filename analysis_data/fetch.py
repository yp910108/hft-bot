"""Fetch historical data from Polymarket public APIs.

支持:
- 用户名解析: @distinct-baguette / 0x8dxd / 0x<地址> 三种入参
- 时间游标分页: 突破 data-api offset=3100 硬上限，可拉到任意深度
"""

import re
import time
import json
import sys
from pathlib import Path
from typing import Union, Optional, List, Dict

import httpx

BASE = "https://data-api.polymarket.com"
CACHE_DIR = Path(__file__).parent / "cache"
CACHE_TTL = 86400  # 1 day
OFFSET_CAP = 3000  # data-api offset 到 3100 即返 400，3000 是最后一个安全整页起点
BATCH_FLUSH = 5    # 每翻 5 页(500 条)落盘一次：进度可见 + 断点续传
REQ_DELAY = 0.25   # 请求间隔(秒)，温和点避免限速


def _cache_path(name: str) -> Path:
    CACHE_DIR.mkdir(exist_ok=True)
    return CACHE_DIR / f"{name}.json"


def _load_cache(name: str, max_age: int = CACHE_TTL) -> Optional[list]:
    p = _cache_path(name)
    if p.exists() and (time.time() - p.stat().st_mtime) < max_age:
        return json.loads(p.read_text())
    return None


def _save_cache(name: str, data):
    _cache_path(name).write_text(json.dumps(data, indent=2))


def _meta_path(name: str) -> Path:
    return CACHE_DIR / f"{name}.meta.json"


def _load_meta(name: str) -> dict:
    p = _meta_path(name)
    if p.exists():
        try:
            return json.loads(p.read_text())
        except Exception:
            return {}
    return {}


def _save_meta(name: str, meta: dict):
    _meta_path(name).write_text(json.dumps(meta))


def _get(client: httpx.Client, path: str, params: Optional[dict] = None) -> list:
    """GET with retry; returns list. Returns empty on 4xx."""
    for attempt in range(3):
        try:
            r = client.get(f"{BASE}{path}", params=params, timeout=30)
            r.raise_for_status()
            data = r.json()
            return data if isinstance(data, list) else [data]
        except httpx.HTTPStatusError as e:
            if e.response.status_code == 429:
                time.sleep(2 ** attempt)
                continue
            if 400 <= e.response.status_code < 500:
                return []
            raise
    return []


def resolve_address(client: httpx.Client, ident: str) -> str:
    """把 @用户名 / 用户名 / 0x地址 解析成钱包地址(小写)。

    0x 开头且 42 位 → 直接返回。
    否则当用户名，抓 polymarket.com/@<name> 页面的 __NEXT_DATA__ 取 proxyWallet。
    """
    ident = ident.strip().lstrip("@")
    if ident.startswith("0x") and len(ident) == 42:
        return ident.lower()

    url = f"https://polymarket.com/@{ident}"
    r = client.get(url, timeout=20, follow_redirects=True)
    r.raise_for_status()
    html = r.text

    # 优先从 __NEXT_DATA__ 里找与用户名匹配的 proxyWallet
    nd = re.search(r'<script id="__NEXT_DATA__"[^>]*>(.*?)</script>', html, re.S)
    if nd:
        try:
            data = json.loads(nd.group(1))
            found = {}

            def walk(o):
                if isinstance(o, dict):
                    if "proxyWallet" in o:
                        for key in ("name", "pseudonym"):
                            nm = o.get(key)
                            if nm:
                                found.setdefault(nm.lower(), o["proxyWallet"])
                    for v in o.values():
                        walk(v)
                elif isinstance(o, list):
                    for x in o:
                        walk(x)

            walk(data)
            if ident.lower() in found:
                return found[ident.lower()].lower()
            if found:
                # 用户名没精确命中，取第一个 proxyWallet
                return next(iter(found.values())).lower()
        except Exception:
            pass

    # 兜底: 页面里第一个 proxyWallet 字段
    m = re.search(r'"proxyWallet"\s*:\s*"(0x[0-9a-fA-F]{40})"', html)
    if m:
        return m.group(1).lower()

    raise ValueError(f"无法从用户名解析地址: {ident}")


def _fetch_paginated(client: httpx.Client, path: str, user: str,
                     cache_name: str, target: Optional[int] = None,
                     skip_cache: bool = False) -> list:
    """分页拉取: 先 offset 翻到 3000，再切 end= 时间游标继续翻。

    - 每 BATCH_FLUSH 页落盘一次(分批写入)：进度可见、中断不丢。
    - 断点续传: 重跑时读已有缓存，从最老一条 ts 往前接着翻。
    - 完成标记: 缓存拉到底/够数时写 meta.complete=True；只有 complete 的缓存
      且未过期才会被当成"拉够了"直接复用，半截缓存只用作续传起点。
    - target: 目标记录数(None=拉全部)。按 transactionHash 去重。
    """
    meta = _load_meta(cache_name)
    cached = _load_cache(cache_name, max_age=10 ** 9)  # 不按时效，自己判
    fresh = _cache_path(cache_name).exists() and \
        (time.time() - _cache_path(cache_name).stat().st_mtime) < CACHE_TTL

    # 已完成且新鲜 → 直接用
    if not skip_cache and cached and meta.get("complete") and fresh:
        return cached

    # 未见底但已够本次 target 且新鲜 → 也直接用，不再打 API
    if not skip_cache and cached and fresh and target and len(cached) >= target:
        return cached

    # 续传: 以已有缓存为起点
    out = list(cached) if (cached and not skip_cache) else []
    seen_tx = set()
    for x in out:
        tx = x.get("transactionHash", "")
        seen_tx.add(tx if tx else json.dumps(x, sort_keys=True))

    limit = 100

    def absorb(batch):
        added = 0
        for x in batch:
            tx = x.get("transactionHash", "")
            key = tx if tx else json.dumps(x, sort_keys=True)
            if key in seen_tx:
                continue
            seen_tx.add(key)
            out.append(x)
            added += 1
        return added

    def flush(complete=False):
        _save_cache(cache_name, out)
        _save_meta(cache_name, {"complete": complete, "count": len(out)})
        tag = " [完成]" if complete else ""
        print(f"    {path} 已落盘 {len(out)} 条{tag}", flush=True)

    # 阶段 1: offset 分页(仅当还没续传数据时才从 offset 头开始)
    page = 0
    if not out:
        offset = 0
        while offset <= OFFSET_CAP:
            batch = _get(client, path, {"user": user, "limit": limit, "offset": offset})
            if not batch:
                flush(complete=True)
                return out
            absorb(batch)
            page += 1
            if page % BATCH_FLUSH == 0:
                flush()
            if len(batch) < limit:
                flush(complete=True)
                return out
            if target and len(out) >= target:
                flush(complete=False)  # 只是本次够了，未见底，留续传余地
                return out
            offset += limit
            time.sleep(REQ_DELAY)

    # 阶段 2: 时间游标(end=已有数据最早 ts - 1)
    end = min(x["timestamp"] for x in out) - 1
    stale_pages = 0
    while True:
        batch = _get(client, path, {"user": user, "limit": limit, "end": end})
        if not batch:
            break
        added = absorb(batch)
        new_end = min(x["timestamp"] for x in batch) - 1
        if new_end >= end:  # 游标卡住，防死循环
            break
        end = new_end
        page += 1
        if page % BATCH_FLUSH == 0:
            flush()
        if added == 0:
            stale_pages += 1
            if stale_pages >= 3:  # 连续 3 页无新记录 → 到底
                break
        else:
            stale_pages = 0
        if target and len(out) >= target:
            flush(complete=False)  # 本次够数，未见底
            return out
        time.sleep(REQ_DELAY)

    flush(complete=True)
    return out


def fetch_activity(client, user, target=None, skip_cache=False):
    return _fetch_paginated(client, "/activity", user,
                            f"activity_{user[:10]}", target, skip_cache)


def fetch_trades(client, user, target=None, skip_cache=False):
    return _fetch_paginated(client, "/trades", user,
                            f"trades_{user[:10]}", target, skip_cache)


def main():
    ident = sys.argv[1] if len(sys.argv) > 1 else "0xe00740bce98a594e26861838885ab310ec3b548c"
    with httpx.Client() as client:
        addr = resolve_address(client, ident)
        print(f"地址: {addr}")
        print("Fetching activity...")
        activity = fetch_activity(client, addr)
        print(f"  {len(activity)} activities")
        print("Fetching trades...")
        trades = fetch_trades(client, addr)
        print(f"  {len(trades)} trades")
    return activity, trades


if __name__ == "__main__":
    main()
