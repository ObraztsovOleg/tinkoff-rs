#!/usr/bin/env python3
"""Read-only candle collector; chart intervals never alter strategy inputs."""
import argparse
import datetime as dt
import fcntl
import importlib.util
import json
import os
from pathlib import Path
import time

SPEC = importlib.util.spec_from_file_location("campaign", Path(__file__).resolve().parents[1] / "sandbox-campaign.py")
campaign = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(campaign)
# enum, retained history, bar duration, maximum API request span, max bars.
# Limits come from contracts/marketdata.proto and T-Bank MarketData FAQ.
INTERVALS = {
    "5s": ("CANDLE_INTERVAL_5_SEC", dt.timedelta(hours=2), 5, dt.timedelta(minutes=200), 2500),
    "10s": ("CANDLE_INTERVAL_10_SEC", dt.timedelta(hours=2), 10, dt.timedelta(minutes=200), 1250),
    "30s": ("CANDLE_INTERVAL_30_SEC", dt.timedelta(hours=12), 30, dt.timedelta(hours=20), 2500),
    "5m": ("CANDLE_INTERVAL_5_MIN", dt.timedelta(days=14), 300, dt.timedelta(days=7), 2400),
    "30m": ("CANDLE_INTERVAL_30_MIN", dt.timedelta(days=14), 1800, dt.timedelta(days=21), 1200),
    "day": ("CANDLE_INTERVAL_DAY", dt.timedelta(days=365), 86400, dt.timedelta(days=2190), 2400),
}
BACKGROUND = {"30m", "day"}


def price(value):
    return int(value.get("units", 0)) + int(value.get("nano", 0)) / 1e9


def windows(start, end, name):
    _, _, seconds, span, limit = INTERVALS[name]
    # Leave space for an inclusive endpoint and align to the candle boundary.
    step = min(span, dt.timedelta(seconds=seconds * (limit - 1)))
    while start < end:
        stop = min(start + step, end)
        if stop - start <= dt.timedelta(seconds=seconds):
            start = stop - dt.timedelta(seconds=seconds * 2)
        yield start, stop
        start = stop


def refresh(api, instrument, name, cache, persist):
    asset = cache.setdefault("assets", {}).setdefault(instrument["uid"], {"ticker": instrument["ticker"], "series": {}})
    asset["ticker"] = instrument["ticker"]
    previous = asset["series"].get(name, {})
    interval, retention, seconds, _, limit = INTERVALS[name]
    end = campaign.now()
    cutoff = end - retention
    start = cutoff
    if previous.get("observed_at"):
        overlap = dt.timedelta(seconds=max(seconds * 2, 5))
        start = max(cutoff, campaign.timestamp(previous["observed_at"]) - overlap)
    merged = {c["time"]: c for c in previous.get("candles", []) if campaign.timestamp(c["time"]) >= cutoff}
    try:
        for beginning, ending in windows(start, end, name):
            response = api.call("MarketDataService/GetCandles", {"instrumentId": instrument["uid"],
                "interval": interval, "from": beginning.isoformat(), "to": ending.isoformat(), "limit": limit})
            for candle in response.get("candles", []):
                if not cutoff <= campaign.timestamp(candle["time"]) <= end:
                    continue
                merged[candle["time"]] = {"time": candle["time"], "open": price(candle["open"]),
                    "high": price(candle["high"]), "low": price(candle["low"]), "close": price(candle["close"]),
                    "volume": int(candle.get("volume", 0)), "complete": bool(candle.get("isComplete"))}
        asset["series"][name] = {"observed_at": end.isoformat(), "candles": sorted(merged.values(), key=lambda c: c["time"]), "error": False}
    except (RuntimeError, ValueError, KeyError):
        failures = min(previous.get("failures", 0) + 1, 6)
        previous.update(error=True, failures=failures, retry_at=(end + dt.timedelta(seconds=min(300, 5 * 2 ** failures))).isoformat())
        asset["series"][name] = previous
        print(f"Chart data temporarily unavailable: {instrument['ticker']} {name}", flush=True)
    persist(cache)


def update(api, state, cache, persist, requested=None, *, background_limit=None):
    instruments = state.get("instruments", []) + ([state["benchmark"]] if state.get("benchmark") else [])
    instruments = {i["uid"]: i for i in instruments}
    current = campaign.now()
    targets = []
    if requested is not None:
        for item in requested.values():
            uid, name = item.get("uid"), item.get("interval")
            if uid in instruments and name in INTERVALS and item.get("expires_at", 0) > current.timestamp():
                targets.append((uid, name, 5))
    targets += [(uid, name, 60) for uid in instruments for name in sorted(BACKGROUND)]
    background_count, visited = 0, set()
    for uid, name, frequency in targets:
        if (uid, name) in visited:
            continue
        visited.add((uid, name))
        previous = cache.get("assets", {}).get(uid, {}).get("series", {}).get(name, {})
        if previous.get("retry_at") and campaign.timestamp(previous["retry_at"]) > current:
            continue
        # Direct calls without requests retain the original full-refresh behavior.
        if requested is not None and previous.get("observed_at") and (current - campaign.timestamp(previous["observed_at"])).total_seconds() < frequency:
            continue
        if frequency == 60 and background_limit is not None:
            if background_count >= background_limit:
                continue
            background_count += 1
        refresh(api, instruments[uid], name, cache, persist)


def read(path, default):
    try:
        return json.loads(path.read_text())
    except (OSError, ValueError):
        return default


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--parent-pid", type=int, required=True)
    args = parser.parse_args()
    os.umask(0o077)
    lock = (campaign.WORK / "market-feed.lock").open("a")
    fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    path = campaign.WORK / "reports/market-chart.json"
    cache = read(path, {"assets": {}})
    api = campaign.API()
    while os.getppid() == args.parent_pid:
        state = campaign.load()
        state["instruments"] = list({i["uid"]: i for i in state.get("instruments", []) + read(campaign.WORK / "dashboard-catalog.json", [])}.values())
        subscriptions = read(campaign.WORK / "chart-subscriptions.json", {})
        update(api, state, cache, lambda data: campaign.save(path, data), subscriptions, background_limit=1)
        time.sleep(0.5)


if __name__ == "__main__":
    main()
