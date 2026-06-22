"""Fetch settlement data (winner) from CLOB API for each market."""

import json
import time
from pathlib import Path
from typing import Optional

import httpx

CACHE_DIR = Path(__file__).parent / "cache"


def fetch_winner(client: httpx.Client, condition_id: str) -> Optional[dict]:
    """Fetch winner info from CLOB API for a given condition_id."""
    cache_file = CACHE_DIR / f"winner_{condition_id[:16]}.json"
    if cache_file.exists():
        return json.loads(cache_file.read_text())

    try:
        r = client.get(f"https://clob.polymarket.com/markets/{condition_id}", timeout=15)
        if r.status_code != 200:
            return None
        data = r.json()
        tokens = data.get("tokens", [])
        winner_idx = None
        for t in tokens:
            if t.get("winner"):
                outcome = t.get("outcome", "")
                winner_idx = 0 if outcome == "Up" else 1

        result = {
            "condition_id": condition_id,
            "winner_idx": winner_idx,
            "winner_outcome": "Up" if winner_idx == 0 else ("Down" if winner_idx == 1 else None),
            "closed": data.get("closed", False),
            "tokens": tokens,
        }
        cache_file.write_text(json.dumps(result))
        return result
    except Exception:
        return None


def batch_fetch_winners(condition_ids: list[str], max_concurrent: int = 5) -> dict[str, dict]:
    """Fetch winner info for multiple markets."""
    results = {}
    with httpx.Client() as client:
        for i, cid in enumerate(condition_ids):
            result = fetch_winner(client, cid)
            if result:
                results[cid] = result
            if (i + 1) % 10 == 0:
                print(f"  Fetched {i + 1}/{len(condition_ids)} winners")
                time.sleep(0.5)
            else:
                time.sleep(0.15)
    return results
