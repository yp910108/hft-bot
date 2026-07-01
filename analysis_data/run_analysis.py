"""
分析任意钱包地址的 Polymarket BTC Up/Down 交易，生成逐场 HTML 报告。

用法:
    python3 run_analysis.py <钱包地址> [--output DIR] [--skip-cache]

流程:
    1. fetch.py     — 从 Polymarket API 拉取 activity + trades
    2. analyze.py   — 按 condition_id 分组、去重、排序
    3. settlement.py — 获取结算数据（谁赢了）
    4. full_analysis — 逐笔模拟 PnL
    5. gen_html.py   — 生成 HTML 报告

数据来源:
    - Polymarket Data API (data-api.polymarket.com): activity, trades, positions
    - Polymarket CLOB API (clob.polymarket.com): 结算数据 (winner)
    - 所有数据通过链上交易记录获取，无需链上直接读取
"""

import sys
import json
import time
import argparse
from pathlib import Path
from collections import defaultdict
from datetime import datetime, timezone

import httpx

# 将当前目录加入 path，以便 import 同级模块
sys.path.insert(0, str(Path(__file__).parent))

from fetch import fetch_activity, fetch_trades, resolve_address
from analyze import build_sessions, _extract_cycle_type
from settlement import batch_fetch_winners


# ---------- 配置 ----------

def parse_args():
    parser = argparse.ArgumentParser(description="分析 Polymarket 钱包交易")
    parser.add_argument("ident", help="钱包地址 (0x...) 或用户名 (@name / name)")
    parser.add_argument("--output", "-o", default=None, help="输出目录 (默认: output/<address>)")
    parser.add_argument("--skip-cache", action="store_true", help="忽略缓存，重新拉取数据")
    parser.add_argument("--no-settle", action="store_true", help="跳过结算数据获取（加快速度，但无 winner 信息）")
    parser.add_argument("--target-5m", type=int, default=3000, help="5分钟目标场数 (默认 3000)")
    parser.add_argument("--target-15m", type=int, default=1000, help="15分钟目标场数 (默认 1000)")
    return parser.parse_args()


# ---------- HTML 生成 ----------

def classify_step(direction, pre_uq, pre_uc, pre_dq, pre_dc, up_q, up_c, dn_q, dn_c):
    pre_total = pre_uc + pre_dc
    if pre_total == 0:
        return "build"
    pre_up = pre_uq - pre_total
    pre_dn = pre_dq - pre_total
    tc = up_c + dn_c
    post_up = up_q - tc
    post_dn = dn_q - tc
    my_pre = pre_up if direction == "UP" else pre_dn
    my_post = post_up if direction == "UP" else post_dn
    if my_pre < 0 and my_post > my_pre:
        return "rescue"
    if pre_up > 0 and pre_dn > 0:
        return "lock"
    return "build"


def action_color(a):
    return {"rescue": "#FF9800", "lock": "#2196F3"}.get(a, "transparent")


def pnl_c(v):
    return "#66bb6a" if v > 0 else "#ef5350" if v < 0 else "#888"


CSS = """body{font-family:'SF Mono','Menlo','Monaco','Courier New',monospace;margin:0;background:#1a1a2e;color:#e0e0e0}
h1{color:#fff;font-size:18px;margin:16px 20px 4px}
.back{display:inline-block;margin:8px 20px;color:#64b5f6;text-decoration:none;font-size:13px}
.back:hover{text-decoration:underline}
.meta{display:grid;grid-template-columns:repeat(auto-fill,minmax(200px,1fr));gap:4px 16px;margin:12px 20px;font-size:13px}
.meta .item{color:#aaa} .meta b{color:#e0e0e0}
.pos{color:#66bb6a} .neg{color:#ef5350}
table{width:calc(100% - 40px);margin:8px 20px 20px;border-collapse:collapse;font-size:12px}
th{background:#0f3460;color:#90caf9;text-align:right;padding:5px 8px;position:sticky;top:0;white-space:nowrap;z-index:1}
th:first-child{text-align:center}
td{text-align:right;padding:4px 8px;border-bottom:1px solid #1a1a3e;white-space:nowrap}
td:first-child{text-align:center;color:#666}
tr:hover{background:#1a2744}
.r{font-size:11px;padding:1px 5px;border-radius:3px;font-weight:bold;color:#fff}
.hedge{font-size:11px;padding:1px 5px;border-radius:3px;color:#fff;font-weight:bold}
.dir-switch{font-weight:bold;color:#FFD54F}
.conv{color:#FFB74D;cursor:help;font-weight:bold}
.result-box{margin:12px 20px;padding:12px 16px;border-radius:6px;display:inline-block}
.result-box.win{background:#1b5e20;border:1px solid #4CAF50}
.result-box.loss{background:#b71c1c;border:1px solid #f44336}
.result-box .big{font-size:28px;font-weight:bold;color:#fff}
.result-box .sub{font-size:13px;color:#ccc;margin-top:4px}"""

INDEX_CSS = """body{font-family:'SF Mono','Menlo','Monaco','Courier New',monospace;margin:0;background:#1a1a2e;color:#e0e0e0}
h1{color:#fff;text-align:center;padding:20px 0 8px;font-size:20px}
.summary{text-align:center;color:#aaa;margin-bottom:16px;font-size:14px} .summary b{color:#fff}
.tabs{display:flex;justify-content:center;gap:0;margin-bottom:16px}
.tab-btn{background:#2a2a4a;color:#aaa;border:1px solid #444;padding:8px 28px;cursor:pointer;font-size:14px;font-weight:bold}
.tab-btn:first-child{border-radius:6px 0 0 6px} .tab-btn:last-child{border-radius:0 6px 6px 0}
.tab-btn:hover{background:#3a3a5a} .tab-btn.active{background:#1565C0;color:#fff;border-color:#1565C0}
.tab-panel{display:none} .tab-panel.active{display:block}
.sub-tabs{display:flex;justify-content:center;gap:8px;margin-bottom:12px}
.sub-btn{background:#2a2a4a;color:#ccc;border:1px solid #444;padding:4px 16px;border-radius:4px;cursor:pointer;font-size:12px}
.sub-btn:hover{background:#3a3a5a} .sub-btn.active{background:#4CAF50;color:#fff;border-color:#4CAF50}
.sub-btn.active.loss-sub{background:#f44336;border-color:#f44336}
.section-hdr{text-align:center;color:#64b5f6;font-size:13px;margin:8px 0 4px}
.grid{display:grid;grid-template-columns:repeat(auto-fill,minmax(320px,1fr));gap:8px;padding:0 16px 20px}
.card{background:#16213e;border-radius:6px;padding:10px 12px;border:1px solid #2a2a4a;cursor:pointer;text-decoration:none;color:inherit;display:block;transition:background .15s}
.card:hover{background:#1a2744}
.card.win{border-left:3px solid #4CAF50} .card.loss{border-left:3px solid #f44336}
.card .title{font-size:12px;color:#ccc;margin-bottom:4px;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
.card .line{font-size:11px;color:#888}
.card .tag{font-size:10px;padding:1px 5px;border-radius:2px;color:#fff;margin-right:4px}"""


def _build_rows(ops):
    """Build table rows from operations."""
    rows = []
    cuq = cuc = cdq = cdc = 0.0
    prev_dir = None
    for j, op in enumerate(ops):
        puq, puc, pdq, pdc = cuq, cuc, cdq, cdc
        d, p, s = op["direction"], op["price"], op["size"]
        if d == "UP":
            cuq += s; cuc += p * s
        else:
            cdq += s; cdc += p * s
        # 如果 JSON 有 engine 算好的真实值就用，否则回退到自己累加的（兼容旧数据）。
        up_qty = op.get("up_qty", cuq)
        up_cost = op.get("up_cost", cuc)
        dn_qty = op.get("dn_qty", cdq)
        dn_cost = op.get("dn_cost", cdc)
        tc = op.get("total_cost", cuc + cdc)
        ua = up_cost / up_qty if up_qty > 0 else 0
        da = dn_cost / dn_qty if dn_qty > 0 else 0
        pu = op.get("pnl_if_up_wins", up_qty - tc)
        pd = op.get("pnl_if_dn_wins", dn_qty - tc)
        ps = pu - pd
        act = classify_step(d, puq, puc, pdq, pdc, cuq, cuc, cdq, cdc)
        sw = prev_dir is not None and d != prev_dir
        sw_h = '<b class="dir-switch">Y</b>' if sw else ""
        act_h = f'<span class="hedge" style="background:{action_color(act)}">{act}</span>' if act != "build" else ""
        t = op["time"][11:] if len(op["time"]) > 11 else op["time"]
        # 卖出转买入对边的行: 加背景色 + 方向旁标 ⇄，并提示原始卖出
        conv = op.get("converted")
        row_style = ' style="background:#3a2e1a"' if conv else ""
        if conv:
            tip = f'卖{op.get("orig_dir")}@{op.get("orig_price"):.4f} → 买{d}@{p:.4f}'
            dir_h = f'<b>{d}</b> <span class="conv" title="{tip}">⇄</span>'
        else:
            dir_color = "#66bb6a" if d == "UP" else "#ef5350"
            dir_h = f'<b style="color:{dir_color}">{d}</b>'
        role = op.get('role', '')
        if role == "Taker":
            role_h = '<span style="background:#e65100;color:#fff;padding:1px 5px;border-radius:3px;font-size:10px">Taker</span>'
        elif role == "Maker":
            role_h = '<span style="background:#1565c0;color:#fff;padding:1px 5px;border-radius:3px;font-size:10px">Maker</span>'
        else:
            role_h = role
        rows.append(f"<tr{row_style}><td>{j+1}</td><td>{t}</td><td>{dir_h}</td>"
                    f"<td>{role_h}</td>"
                    f"<td>{p:.4f}</td><td>{s:.1f}</td>"
                    f"<td>{up_qty:.1f}</td><td>{up_cost:.2f}</td><td>{ua:.4f}</td>"
                    f"<td>{dn_qty:.1f}</td><td>{dn_cost:.2f}</td><td>{da:.4f}</td>"
                    f"<td>{tc:.2f}</td>"
                    f'<td style="color:{pnl_c(pu)}">{pu:+.2f}</td>'
                    f'<td style="color:{pnl_c(pd)}">{pd:+.2f}</td>'
                    f'<td style="color:{pnl_c(ps)}">{ps:+.2f}</td>'
                    f"<td>{sw_h}</td><td>{act_h}</td></tr>")
        prev_dir = d
    return rows


def render_market(m, idx, total):
    f = m["final"]
    pnl = f.get("actual_pnl")
    if pnl is None:
        pnl = 0
    winner = m.get("winner", "?")
    up_qty, up_cost = f["up_qty"], f["up_cost"]
    dn_qty, dn_cost = f["dn_qty"], f["dn_cost"]
    total_cost = f["total_cost"]
    up_avg = up_cost / up_qty if up_qty > 0 else 0
    dn_avg = dn_cost / dn_qty if dn_qty > 0 else 0
    sum_avg = up_avg + dn_avg
    cycle = m["cycle_type"]
    short = m["title"].replace("Bitcoin Up or Down - ", "")
    result = "WIN" if pnl > 0 else "LOSS"
    source_file = m.get("source_file", "")

    prev_file = f"m{idx-1:03d}.html" if idx > 0 else "index.html"
    next_file = f"m{idx+1:03d}.html" if idx < total - 1 else "index.html"

    rows = _build_rows(m["operations"])

    return f"""<!DOCTYPE html><html lang="zh-CN"><head><meta charset="UTF-8">
<title>#{idx+1} {short}</title><style>{CSS}</style></head><body>
<a class="back" href="index.html">[索引]</a>
<a class="back" href="{prev_file}">[← 上一场]</a>
<a class="back" href="{next_file}">[下一场 →]</a>
<h1>#{idx+1}/{total} {short}
<span style="font-size:13px;padding:2px 8px;border-radius:3px;background:{'#1565C0' if cycle=='5m' else '#6A1B9A'};color:#fff">{cycle}</span>
</h1>
{f'<div style="margin:0 20px 8px;font-size:12px;color:#888">源文件: <code style="color:#4FC3F7">{source_file}</code></div>' if source_file else ''}
<div class="result-box {"win" if pnl>0 else "loss"}">
<div class="big">{winner}赢 → PnL {pnl:+.2f}</div>
<div class="sub">总成本 ${total_cost:.2f} | UP赢 {f["pnl_if_up"]:+.2f} | DN赢 {f["pnl_if_dn"]:+.2f} | sum_avg {sum_avg:.4f}</div>
</div>
<div class="meta">
<div class="item">UP持仓: <b>{up_qty:.1f}</b> 个 (cost=${up_cost:.2f}, avg={up_avg:.4f})</div>
<div class="item">DN持仓: <b>{dn_qty:.1f}</b> 个 (cost=${dn_cost:.2f}, avg={dn_avg:.4f})</div>
<div class="item">操作笔数: <b>{m["num_trades"]}</b></div>
</div>
<div style="margin:4px 20px;font-size:12px;color:#FFB74D">
⇄ 标记行 = 卖出已等价转换为买入对边(卖S@p ≡ 买¬S@1-p)，悬停查看原始卖出；无标记行为原始买入。
</div>
<table>
<tr><th>#</th><th>时间</th><th>方向</th><th>角色</th><th>价格</th><th>数量</th>
<th>UP持仓</th><th>UP成本</th><th>UP均价</th><th>DN持仓</th><th>DN成本</th><th>DN均价</th>
<th>总成本</th><th>UP赢PnL</th><th>DN赢PnL</th><th>PnL差</th><th>换向</th><th>动作</th></tr>
{"".join(rows)}
</table></body></html>"""


def _card(m, i):
    f = m["final"]
    pnl = f.get("actual_pnl") or 0
    r = "win" if pnl > 0 else "loss"
    short = m["title"].replace("Bitcoin Up or Down - ", "")
    sum_avg = (f["up_cost"]/f["up_qty"] if f["up_qty"]>0 else 0) + (f["dn_cost"]/f["dn_qty"] if f["dn_qty"]>0 else 0)
    winner = m.get("winner", "?")
    return f"""<a href="m{i:03d}.html" class="card {r}">
<div class="title">#{i+1} {short}</div>
<div class="line">
<span class="tag" style="background:{'#2e7d32' if pnl>0 else '#c62828'}">{pnl:+.0f}</span>
<span class="tag" style="background:#555">{winner}赢</span>
<span class="tag" style="background:{'#2e7d32' if sum_avg<1 else '#c62828'}">sa={sum_avg:.3f}</span>
ops={m["num_trades"]} cost=${f["total_cost"]:.0f}
</div></a>"""


def _sub_section(markets, indices, result_filter=None):
    items = [(i, m) for i, m in zip(indices, markets)
             if result_filter is None or
             (result_filter == "win" and (m["final"].get("actual_pnl") or 0) > 0) or
             (result_filter == "loss" and (m["final"].get("actual_pnl") or 0) <= 0)]
    if not items:
        return '<div class="section-hdr" style="color:#666">无数据</div>'
    sub_pnl = sum(m["final"].get("actual_pnl") or 0 for _, m in items)
    hdr = f'{len(items)} 场, 总PnL {sub_pnl:+,.0f}'
    return f'<div class="section-hdr">{hdr}</div><div class="grid">{"".join(_card(m, i) for i, m in items)}</div>'


def render_index(markets, address):
    total_pnl = sum(m["final"].get("actual_pnl") or 0 for m in markets)
    wins = sum(1 for m in markets if (m["final"].get("actual_pnl") or 0) > 0)
    n = len(markets)

    idx_5m = [i for i, m in enumerate(markets) if m["cycle_type"] == "5m"]
    idx_15m = [i for i, m in enumerate(markets) if m["cycle_type"] != "5m"]
    w5 = sum(1 for i in idx_5m if (markets[i]["final"].get("actual_pnl") or 0) > 0)
    w15 = sum(1 for i in idx_15m if (markets[i]["final"].get("actual_pnl") or 0) > 0)
    p5 = sum(markets[i]["final"].get("actual_pnl") or 0 for i in idx_5m)
    p15 = sum(markets[i]["final"].get("actual_pnl") or 0 for i in idx_15m)

    tabs = []
    if idx_15m:
        tabs.append(f"""<div class="section-hdr" style="font-size:15px;color:#fff">
{len(idx_15m)} 场 | 总PnL {p15:+,.0f} | 胜率 {w15}/{len(idx_15m)} ({w15/len(idx_15m)*100:.0f}%)</div>
<div class="sub-tabs">
<button onclick="sf(this,'15m','all')" class="sub-btn active">全部 ({len(idx_15m)})</button>
<button onclick="sf(this,'15m','win')" class="sub-btn">盈利 ({w15})</button>
<button onclick="sf(this,'15m','loss')" class="sub-btn loss-sub">亏损 ({len(idx_15m)-w15})</button>
</div>
<div id="15m-all">{_sub_section(markets, idx_15m)}</div>
<div id="15m-win" style="display:none">{_sub_section(markets, idx_15m, "win")}</div>
<div id="15m-loss" style="display:none">{_sub_section(markets, idx_15m, "loss")}</div>""")

    if idx_5m:
        tabs.append(f"""<div class="section-hdr" style="font-size:15px;color:#fff">
{len(idx_5m)} 场 | 总PnL {p5:+,.0f} | 胜率 {w5}/{len(idx_5m)} ({w5/len(idx_5m)*100:.0f}%)</div>
<div class="sub-tabs">
<button onclick="sf(this,'5m','all')" class="sub-btn active">全部 ({len(idx_5m)})</button>
<button onclick="sf(this,'5m','win')" class="sub-btn">盈利 ({w5})</button>
<button onclick="sf(this,'5m','loss')" class="sub-btn loss-sub">亏损 ({len(idx_5m)-w5})</button>
</div>
<div id="5m-all">{_sub_section(markets, idx_5m)}</div>
<div id="5m-win" style="display:none">{_sub_section(markets, idx_5m, "win")}</div>
<div id="5m-loss" style="display:none">{_sub_section(markets, idx_5m, "loss")}</div>""")

    tab_btns = ""
    panels = []
    if idx_15m:
        tab_btns += '<button class="tab-btn active" onclick="st(\'15m\')">15分钟 ({0})</button>'.format(len(idx_15m))
        panels.append(f'<div id="panel-15m" class="tab-panel active">{tabs[0]}</div>')
    if idx_5m:
        cls = " active" if not idx_15m else ""
        tab_btns += f'<button class="tab-btn{cls}" onclick="st(\'5m\')">5分钟 ({len(idx_5m)})</button>'
        panels.append(f'<div id="panel-5m" class="tab-panel{" active" if not idx_15m else ""}">{tabs[-1]}</div>')

    return f"""<!DOCTYPE html><html lang="zh-CN"><head><meta charset="UTF-8">
<title>Polymarket 交易分析 — {address[:10]}...</title><style>{INDEX_CSS}</style></head><body>
<h1>Polymarket 交易分析 — {address[:10]}...{address[-4:]}</h1>
<div class="summary">
总PnL: <b style="color:{'#66bb6a' if total_pnl>0 else '#ef5350'}">{total_pnl:+,.0f}</b> |
胜率: <b>{wins}/{n} ({wins/n*100:.1f}%)</b> |
场均: <b style="color:{'#66bb6a' if total_pnl/n>0 else '#ef5350'}">{total_pnl/n:+,.1f}</b>
</div>
<div class="tabs">{tab_btns}</div>
{"".join(panels)}
<script>
function st(t){{
  document.querySelectorAll('.tab-btn').forEach(b=>b.classList.remove('active'));
  event.target.classList.add('active');
  document.querySelectorAll('.tab-panel').forEach(p=>p.classList.remove('active'));
  document.getElementById('panel-'+t).classList.add('active');
}}
function sf(btn,cycle,sub){{
  btn.parentElement.querySelectorAll('.sub-btn').forEach(b=>b.classList.remove('active'));
  btn.classList.add('active');
  ['all','win','loss'].forEach(s=>{{
    var el=document.getElementById(cycle+'-'+s);
    if(el) el.style.display = s===sub ? '' : 'none';
  }});
}}
</script></body></html>"""


# ---------- 主流程 ----------

def run(ident, output_dir, skip_cache=False, no_settle=False, target_5m=3000, target_15m=1000):
    # 解析 @用户名 / 用户名 / 0x地址 → 钱包地址
    with httpx.Client() as client:
        address = resolve_address(client, ident)
    if not output_dir:
        output_dir = Path(__file__).parent / "output" / address[:10]
    output_dir = Path(output_dir)
    html_dir = output_dir / "markets_html"
    html_dir.mkdir(parents=True, exist_ok=True)

    print(f"=== 分析: {ident} → {address} ===")
    print(f"输出目录: {output_dir}")

    # Step 1: 获取数据
    # 目标场数粗略换算成需拉取的成交条数(每场 ~6-10 笔，留足余量翻深)
    target_records = (target_5m + target_15m) * 12
    print("\n--- Step 1: 获取交易数据 ---")
    with httpx.Client() as client:
        activity = fetch_activity(client, user=address, target=target_records, skip_cache=skip_cache)
        trades = fetch_trades(client, user=address, target=target_records, skip_cache=skip_cache)
        print(f"  activity: {len(activity)} 条, trades: {len(trades)} 条")

    if not activity and not trades:
        print("ERROR: 未获取到任何数据。请检查地址/用户名是否正确。")
        return

    # Step 2: 构建场次
    print("\n--- Step 2: 构建场次 ---")
    sessions = build_sessions(activity, trades)
    print(f"  总场次: {len(sessions)}")

    # 筛选 5m/15m BTC 场次（有买卖操作的）
    selected = []
    for cid, s in sessions.items():
        has_up = any(op.outcome_idx == 0 and op.side == 'BUY' and op.type == 'TRADE' for op in s.ops)
        has_dn = any(op.outcome_idx == 1 and op.side == 'BUY' and op.type == 'TRADE' for op in s.ops)
        if not has_up and not has_dn:
            continue
        cycle = _extract_cycle_type(s.title)
        if cycle not in ('5m', '15m'):
            continue
        s.cycle_type = cycle
        first_ts = min((op.timestamp for op in s.ops if op.type == 'TRADE'), default=0)
        selected.append((first_ts, cid, s))

    selected.sort()
    print(f"  5m/15m 场次: {len(selected)}")

    # Step 3: 获取结算数据
    cids = [cid for _, cid, _ in selected]
    winners = {}
    if not no_settle:
        print("\n--- Step 3: 获取结算数据 ---")
        winners = batch_fetch_winners(cids)
        settled = sum(1 for w in winners.values() if w.get("winner_idx") is not None)
        print(f"  已结算: {settled}/{len(cids)}")
    else:
        print("\n--- Step 3: 跳过结算数据 ---")

    # Step 4: 逐场分析
    print("\n--- Step 4: 逐场分析 ---")
    results = []
    for first_ts, cid, s in selected:
        winner_info = winners.get(cid, {})
        winner_idx = winner_info.get("winner_idx")

        ops_detail = []
        up_qty = up_cost = dn_qty = dn_cost = 0.0

        for op in s.ops:
            if op.type != 'TRADE':
                continue
            price, size, idx = op.price, op.size, op.outcome_idx
            if idx not in (0, 1):
                continue

            # 卖出等价转买入对边: 卖 S@p ≡ 买 ¬S@(1-p)，结算 PnL 完全等价。
            # 统一成纯买入口径，修正旧"SELL 减成本+max(0)"漏算卖出收益的问题。
            if op.side == 'SELL':
                eff_idx = 1 - idx          # 买入的边
                eff_price = 1.0 - price    # 等价买入价
                converted = True
                orig_dir = "UP" if idx == 0 else "DN"  # 原始卖出的方向
            else:
                eff_idx = idx
                eff_price = price
                converted = False
                orig_dir = None

            if eff_idx == 0:
                up_qty += size; up_cost += eff_price * size
            else:
                dn_qty += size; dn_cost += eff_price * size

            total_cost = up_cost + dn_cost
            ops_detail.append({
                "time": op.dt,
                "direction": "UP" if eff_idx == 0 else "DN",
                "side": "BUY",  # 转换后统一为买入
                "price": round(eff_price, 6),
                "size": round(size, 4),
                "converted": converted,
                "orig_dir": orig_dir,
                "orig_price": round(price, 6) if converted else None,
                "up_qty": round(up_qty, 4),
                "up_cost": round(up_cost, 4),
                "dn_qty": round(dn_qty, 4),
                "dn_cost": round(dn_cost, 4),
                "total_cost": round(total_cost, 4),
                "pnl_if_up_wins": round(up_qty - total_cost, 4),
                "pnl_if_dn_wins": round(dn_qty - total_cost, 4),
            })

        final = ops_detail[-1] if ops_detail else None
        final_pnl = None
        if final and winner_idx is not None:
            final_pnl = final["pnl_if_up_wins"] if winner_idx == 0 else final["pnl_if_dn_wins"]

        results.append({
            "condition_id": cid,
            "title": s.title,
            "cycle_type": s.cycle_type,
            "winner": "UP" if winner_idx == 0 else ("DN" if winner_idx == 1 else None),
            "winner_idx": winner_idx,
            "settled": winner_idx is not None,
            "num_trades": len(ops_detail),
            "operations": ops_detail,
            "final": {
                "up_qty": final["up_qty"] if final else 0,
                "up_cost": final["up_cost"] if final else 0,
                "dn_qty": final["dn_qty"] if final else 0,
                "dn_cost": final["dn_cost"] if final else 0,
                "total_cost": final["total_cost"] if final else 0,
                "pnl_if_up": final["pnl_if_up_wins"] if final else 0,
                "pnl_if_dn": final["pnl_if_dn_wins"] if final else 0,
                "actual_pnl": round(final_pnl, 4) if final_pnl is not None else None,
            },
        })

    # 保存 JSON
    json_file = output_dir / "full_analysis.json"
    json_file.write_text(json.dumps(results, indent=2, ensure_ascii=False))
    print(f"  JSON -> {json_file}")

    # Step 5: 生成 HTML
    print("\n--- Step 5: 生成 HTML ---")
    n = len(results)
    for i, m in enumerate(results):
        html = render_market(m, i, n)
        (html_dir / f"m{i:03d}.html").write_text(html, encoding="utf-8")
        if (i + 1) % 50 == 0:
            print(f"  {i+1}/{n}")

    (html_dir / "index.html").write_text(render_index(results, address), encoding="utf-8")
    print(f"\n完成! {n} 场 HTML 已生成")
    print(f"打开: {html_dir / 'index.html'}")

    # 统计摘要
    settled = [r for r in results if r["settled"]]
    if settled:
        wins = sum(1 for r in settled if (r["final"]["actual_pnl"] or 0) > 0)
        total_pnl = sum(r["final"]["actual_pnl"] or 0 for r in settled)
        print(f"\n摘要: {len(settled)} 场已结算 | 胜率 {wins}/{len(settled)} ({wins/len(settled)*100:.1f}%) | 总PnL {total_pnl:+,.0f}")

    return results


if __name__ == "__main__":
    args = parse_args()
    run(args.ident, args.output, args.skip_cache, args.no_settle,
        args.target_5m, args.target_15m)
