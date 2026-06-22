"""
从 Rust 回测产出的 full_analysis.json 渲染 HTML 报告。

复用 run_analysis.py 的 render_market / render_index 函数,
产出和分析真实玩家时一模一样的逐场 HTML 报告。

用法:
    python3 analysis_data/render_rust_report.py [--input JSON] [--output DIR]

默认:
    --input  analysis_data/output/rust_strategy/full_analysis.json
    --output analysis_data/output/rust_strategy/markets_html/
"""

import sys
import json
import argparse
from pathlib import Path

# 把 analysis_data 加入 path 以 import run_analysis 的渲染函数。
sys.path.insert(0, str(Path(__file__).parent))

from run_analysis import render_market, render_index


def parse_args():
    parser = argparse.ArgumentParser(description="从 Rust 回测 JSON 渲染 HTML 报告")
    parser.add_argument(
        "--input", "-i",
        default="analysis_data/output/rust_strategy/full_analysis.json",
        help="输入 JSON 文件路径",
    )
    parser.add_argument(
        "--output", "-o",
        default=None,
        help="HTML 输出目录 (默认: JSON 同级 markets_html/)",
    )
    return parser.parse_args()


def main():
    args = parse_args()
    input_path = Path(args.input)
    if not input_path.exists():
        print(f"错误: 找不到 {input_path}")
        print("请先运行: cargo run -p backtest --example report_json --release")
        sys.exit(1)

    data = json.loads(input_path.read_text())
    print(f"加载 {len(data)} 场回测记录")

    # 输出目录。
    if args.output:
        html_dir = Path(args.output)
    else:
        html_dir = input_path.parent / "markets_html"
    html_dir.mkdir(parents=True, exist_ok=True)

    # 渲染逐场 HTML。
    n = len(data)
    for i, m in enumerate(data):
        html = render_market(m, i, n)
        (html_dir / f"m{i:03d}.html").write_text(html, encoding="utf-8")
        if (i + 1) % 100 == 0:
            print(f"  已渲染 {i + 1}/{n}")

    # 渲染索引页。
    address = "rust_strategy"
    (html_dir / "index.html").write_text(render_index(data, address), encoding="utf-8")

    # 统计摘要。
    settled = [r for r in data if r.get("settled")]
    wins = sum(1 for r in settled if (r["final"].get("actual_pnl") or 0) > 0)
    total_pnl = sum(r["final"].get("actual_pnl") or 0 for r in settled)
    print(f"\n完成! {n} 场 HTML 已生成")
    print(f"摘要: 胜率 {wins}/{len(settled)} ({wins/len(settled)*100:.1f}%) | 总PnL {total_pnl:+,.0f} | 场均 {total_pnl/len(settled):+,.1f}")
    print(f"\n打开报告: {html_dir / 'index.html'}")


if __name__ == "__main__":
    main()
