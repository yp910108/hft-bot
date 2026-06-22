"""Analyze trader data: group by market, reconstruct sequences, simulate PnL."""

from collections import defaultdict
from dataclasses import dataclass, field
from datetime import datetime, timezone
import re

TAKER_FEE = 0.04


def _safe_oi(val):
    """Safely parse outcomeIndex, handling None and falsy 0."""
    if val is None:
        return -1
    try:
        v = int(val)
        return v if v in (0, 1) else -1
    except (ValueError, TypeError):
        return -1


@dataclass
class Op:
    idx: int
    timestamp: int       # epoch seconds
    dt: str              # ISO format
    type: str            # TRADE, REDEEM
    side: str            # BUY, SELL
    price: float
    size: float
    outcome: str         # Up, Down
    outcome_idx: int     # 0=Up, 1=Down
    title: str
    tx_hash: str
    raw: dict = field(repr=False)


@dataclass
class MarketSession:
    condition_id: str
    title: str = ""
    cycle_type: str = ""  # 5m, 15m, 1h, unknown
    ops: list[Op] = field(default_factory=list)
    positions: list[dict] = field(default_factory=list)

    def add(self, op: Op):
        self.ops.append(op)

    def sort_ops(self):
        self.ops.sort(key=lambda o: o.timestamp)


def _epoch_to_iso(ts) -> str:
    if isinstance(ts, (int, float)):
        return datetime.fromtimestamp(ts, tz=timezone.utc).isoformat()[:19]
    return str(ts)[:19]


def _extract_cycle_type(title: str) -> str:
    """Extract cycle type from market title like '9:00AM-9:05AM' → 5m."""
    m = re.search(r'(\d{1,2}:\d{2}(?:AM|PM))\s*-\s*(\d{1,2}:\d{2}(?:AM|PM))', title)
    if not m:
        # Check for "12AM ET" style (hourly)
        if re.search(r'\d{1,2}(?:AM|PM)\s+ET$', title):
            return "1h"
        return "unknown"

    def parse_time(t: str) -> int:
        m2 = re.match(r'(\d{1,2}):(\d{2})(AM|PM)', t)
        if not m2:
            return 0
        h, mi, ap = int(m2.group(1)), int(m2.group(2)), m2.group(3)
        if ap == "PM" and h != 12:
            h += 12
        elif ap == "AM" and h == 12:
            h = 0
        return h * 60 + mi

    start = parse_time(m.group(1))
    end = parse_time(m.group(2))
    diff = abs(end - start)
    if diff <= 0:
        return "unknown"
    if diff <= 6:
        return "5m"
    if diff <= 16:
        return "15m"
    if diff <= 31:
        return "30m"
    return "1h"


def build_sessions(activity: list[dict], trades: list[dict]) -> dict[str, MarketSession]:
    """Build market sessions from activity + trades, deduplicating by tx_hash."""
    sessions: dict[str, MarketSession] = {}
    seen_tx = set()

    all_records = []

    for a in activity:
        cid = a.get("conditionId", "")
        if not cid:
            continue
        tx = a.get("transactionHash", "")
        rec = {
            "timestamp": a.get("timestamp", 0),
            "type": a.get("type", "TRADE"),
            "side": a.get("side", ""),
            "price": float(a.get("price", 0) or 0),
            "size": abs(float(a.get("size", 0) or 0)),
            "outcome": a.get("outcome", ""),
            "outcome_idx": _safe_oi(a.get("outcomeIndex")),
            "condition_id": cid,
            "title": a.get("title", ""),
            "tx_hash": tx,
            "raw": a,
        }
        all_records.append(rec)

    for t in trades:
        cid = t.get("conditionId", "")
        if not cid:
            continue
        tx = t.get("transactionHash", "")
        rec = {
            "timestamp": t.get("timestamp", 0),
            "type": "TRADE",
            "side": t.get("side", ""),
            "price": float(t.get("price", 0) or 0),
            "size": abs(float(t.get("size", 0) or 0)),
            "outcome": t.get("outcome", ""),
            "outcome_idx": _safe_oi(t.get("outcomeIndex")),
            "condition_id": cid,
            "title": t.get("title", ""),
            "tx_hash": tx,
            "raw": t,
        }
        all_records.append(rec)

    # Sort by timestamp
    all_records.sort(key=lambda r: r["timestamp"])

    for rec in all_records:
        # Deduplicate by tx_hash
        tx = rec["tx_hash"]
        if tx and tx in seen_tx:
            continue
        if tx:
            seen_tx.add(tx)

        cid = rec["condition_id"]
        if cid not in sessions:
            title = rec.get("title", "")
            sessions[cid] = MarketSession(
                condition_id=cid,
                title=title,
                cycle_type=_extract_cycle_type(title),
            )

        ts = rec["timestamp"]
        op = Op(
            idx=len(sessions[cid].ops),
            timestamp=ts if isinstance(ts, int) else int(ts),
            dt=_epoch_to_iso(ts),
            type=rec["type"],
            side=rec["side"],
            price=rec["price"],
            size=rec["size"],
            outcome=rec["outcome"],
            outcome_idx=rec["outcome_idx"],
            title=rec.get("title", ""),
            tx_hash=tx,
            raw=rec["raw"],
        )
        sessions[cid].add(op)

    for s in sessions.values():
        s.sort_ops()

    return sessions


def enrich_from_positions(sessions: dict[str, MarketSession], positions: list[dict]):
    """Fill in position data, winner detection, and realized PnL."""
    for p in positions:
        cid = p.get("conditionId", "")
        if cid and cid in sessions:
            sessions[cid].positions.append(p)


def get_session_pnl(session: MarketSession) -> dict:
    """Get PnL from positions data (ground truth)."""
    result = {
        "condition_id": session.condition_id,
        "title": session.title,
        "cycle_type": session.cycle_type,
        "up_pnl": 0.0,
        "dn_pnl": 0.0,
        "total_pnl": 0.0,
        "up_size": 0.0,
        "dn_size": 0.0,
        "up_avg_price": 0.0,
        "dn_avg_price": 0.0,
        "has_up": False,
        "has_dn": False,
        "dual_side": False,
        "settled": False,
        "winner": None,
        "realized_pnl": 0.0,
    }

    for p in session.positions:
        oi = int(p.get("outcomeIndex", -1) or -1)
        cash_pnl = float(p.get("cashPnl", 0) or 0)
        size = float(p.get("size", 0) or 0)
        avg_price = float(p.get("avgPrice", 0) or 0)
        cur_price = float(p.get("curPrice", 0) or 0)
        redeemable = p.get("redeemable", False)

        if oi == 0:
            result["up_pnl"] = cash_pnl
            result["up_size"] = size
            result["up_avg_price"] = avg_price
            result["has_up"] = True
            if cur_price >= 0.99:
                result["winner"] = 0  # UP won
            if redeemable:
                result["settled"] = True
                result["realized_pnl"] += cash_pnl
        elif oi == 1:
            result["dn_pnl"] = cash_pnl
            result["dn_size"] = size
            result["dn_avg_price"] = avg_price
            result["has_dn"] = True
            if cur_price >= 0.99:
                result["winner"] = 1  # DN won
            if redeemable:
                result["settled"] = True
                result["realized_pnl"] += cash_pnl

    result["dual_side"] = result["has_up"] and result["has_dn"]
    result["total_pnl"] = result["up_pnl"] + result["dn_pnl"]

    # For non-settled markets, use cashPnl as current PnL
    if not result["settled"]:
        result["realized_pnl"] = result["total_pnl"]

    return result


def simulate_pnl_at_each_step(session: MarketSession) -> list[dict]:
    """Simulate cumulative PnL at each trade step."""
    results = []
    up_qty = 0.0
    up_cost = 0.0
    dn_qty = 0.0
    dn_cost = 0.0

    for op in session.ops:
        if op.type != "TRADE":
            continue

        is_buy = op.side == "BUY"
        price = op.price
        size = op.size
        idx = op.outcome_idx

        if idx == 0:  # UP
            if is_buy:
                up_qty += size
                up_cost += price * size
            else:
                up_qty = max(0, up_qty - size)
                up_cost = max(0, up_cost - price * size)
        elif idx == 1:  # DN
            if is_buy:
                dn_qty += size
                dn_cost += price * size
            else:
                dn_qty = max(0, dn_qty - size)
                dn_cost = max(0, dn_cost - price * size)

        total_cost = up_cost + dn_cost
        pnl_up = up_qty - total_cost
        pnl_dn = dn_qty - total_cost

        results.append({
            "op_idx": op.idx,
            "timestamp": op.dt,
            "side": op.side,
            "outcome": op.outcome,
            "outcome_idx": idx,
            "price": round(price, 6),
            "size": round(size, 4),
            "up_qty": round(up_qty, 4),
            "up_cost": round(up_cost, 4),
            "dn_qty": round(dn_qty, 4),
            "dn_cost": round(dn_cost, 4),
            "total_cost": round(total_cost, 4),
            "pnl_if_up": round(pnl_up, 4),
            "pnl_if_dn": round(pnl_dn, 4),
            "imbalance": round(abs(up_cost - dn_cost), 4),
        })

    return results


def analyze_time_pattern(session: MarketSession) -> dict:
    """Analyze timing patterns of trades in a session."""
    timestamps = [op.timestamp for op in session.ops if op.type == "TRADE"]
    if len(timestamps) < 2:
        return {"num_trades": len(timestamps), "duration_sec": 0, "avg_interval_sec": 0}

    duration = max(timestamps) - min(timestamps)
    intervals = [timestamps[i] - timestamps[i-1] for i in range(1, len(timestamps))]
    avg_interval = sum(intervals) / len(intervals) if intervals else 0

    return {
        "num_trades": len(timestamps),
        "duration_sec": round(duration, 1),
        "avg_interval_sec": round(avg_interval, 1),
        "start": _epoch_to_iso(min(timestamps)),
        "end": _epoch_to_iso(max(timestamps)),
    }


def classify_pattern(session: MarketSession) -> dict:
    """Classify trading pattern."""
    buys_up = 0
    buys_dn = 0
    sells_up = 0
    sells_dn = 0
    redeems = 0

    for op in session.ops:
        if op.type == "REDEEM":
            redeems += 1
        elif op.type == "TRADE":
            if op.side == "BUY":
                if op.outcome_idx == 0:
                    buys_up += 1
                elif op.outcome_idx == 1:
                    buys_dn += 1
            elif op.side == "SELL":
                if op.outcome_idx == 0:
                    sells_up += 1
                elif op.outcome_idx == 1:
                    sells_dn += 1

    has_sell = (sells_up + sells_dn) > 0
    dual_buy = buys_up > 0 and buys_dn > 0

    if has_sell:
        pattern = "buy_and_sell"
    elif dual_buy:
        pattern = "dual_side_buy"
    elif buys_up > 0:
        pattern = "single_up"
    elif buys_dn > 0:
        pattern = "single_dn"
    else:
        pattern = "redeem_only"

    return {
        "pattern": pattern,
        "buys_up": buys_up,
        "buys_dn": buys_dn,
        "sells": sells_up + sells_dn,
        "redeems": redeems,
        "has_sell": has_sell,
        "dual_buy": dual_buy,
    }
