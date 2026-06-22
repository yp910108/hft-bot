# Step 1: Rust 跑回测产出 JSON
cargo run -p backtest --example report_json --release

# Step 2: Python 渲染 HTML
python3 analysis_data/render_rust_report.py