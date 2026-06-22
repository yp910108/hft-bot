# Polymarket 对冲交易分析工具

分析任意 Polymarket 钱包地址在 BTC Up/Down 市场的对冲交易，生成逐场 HTML 报告。

## 环境依赖

- Python 3.10+
- `httpx`

```bash
pip install -r requirements.txt
```

## 运行

```bash
# 分析指定钱包地址
python3 run_analysis.py 0xe00740bce98a594e26861838885ab310ec3b548c

# 自定义输出目录
python3 run_analysis.py 0xe00740bce98a594e26861838885ab310ec3b548c -o my_output

# 忽略缓存重新拉数据
python3 run_analysis.py 0xe00740bce98a594e26861838885ab310ec3b548c --skip-cache

# 跳过结算数据获取（更快，但无 winner 信息）
python3 run_analysis.py 0xe00740bce98a594e26861838885ab310ec3b548c --no-settle
```

## 输出文件结构

```
output/<address>/
├── full_analysis.json          # 全量数据 JSON
├── cache/                      # API 数据缓存
│   ├── activity_0x04b6.json
│   ├── trades_0x04b6.json
│   └── winner_xxxx.json        # 每场结算缓存
└── markets_html/               # HTML 报告
    ├── index.html              # 索引页（入口，15m/5m 两个 Tab）
    ├── m000.html               # 第 1 场详情
    ├── m001.html               # 第 2 场详情
    └── ...
```

## HTML 报告

**index.html**：两个大 Tab（15分钟 / 5分钟），每个 Tab 内有全部/盈利/亏损筛选。

**mXXX.html**：逐场详情，顶部标注 UP赢/DN赢 和 PnL，下方逐笔操作表格。

## 数据来源

- **Data API** (`data-api.polymarket.com`)：activity、trades
- **CLOB API** (`clob.polymarket.com`)：结算数据（winner）
- 数据缓存到本地，24 小时内不重复请求
- 不需要 API key

## 文件说明

```
hedging_analysis/
├── run_analysis.py  # 主入口（一条命令跑完）
├── fetch.py         # 数据获取（API 调用 + 缓存）
├── analyze.py       # 数据分组与场次重建
├── settlement.py    # 结算数据获取
├── gen_html.py      # HTML 报告生成辅助
├── requirements.txt
└── README.md
```
