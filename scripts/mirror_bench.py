#!/usr/bin/env python3
"""Python mirror of the Rust benchmark.

The execution container used for this response does not include a Rust
compiler, so this script exercises the same algorithmic path as the Rust
prototype: build an immutable capsule, hydrate missing reads as read traps, and
commit mutations through one OCC-protected MVCC store. It is not a substitute
for `cargo run --release --bin aster_bench`; it provides reproducible numbers
from this environment for the analysis document.
"""

from __future__ import annotations

import argparse
import json
import statistics
import time
from dataclasses import dataclass, field
from typing import Dict, List, Optional, Tuple

Document = Dict[str, int]


@dataclass
class VersionedDocument:
    version: Optional[int]
    document: Optional[Document]


@dataclass
class Capsule:
    ts: int
    docs: Dict[str, VersionedDocument] = field(default_factory=dict)

    def get(self, key: str) -> Optional[VersionedDocument]:
        return self.docs.get(key)

    def hydrate(self, key: str, value: VersionedDocument) -> None:
        self.docs[key] = value


class Mvcc:
    def __init__(self) -> None:
        self.now = 0
        self.docs: Dict[str, List[Tuple[int, Optional[Document]]]] = {}

    def seed(self, key: str, document: Document) -> int:
        self.now += 1
        self.docs.setdefault(key, []).append((self.now, dict(document)))
        return self.now

    def snapshot_ts(self) -> int:
        return self.now

    def read_at(self, key: str, ts: int) -> VersionedDocument:
        for version, doc in reversed(self.docs.get(key, [])):
            if version <= ts:
                return VersionedDocument(version, None if doc is None else dict(doc))
        return VersionedDocument(None, None)

    def build_capsule(self, ts: int, keys: List[str]) -> Capsule:
        capsule = Capsule(ts)
        for key in keys:
            capsule.hydrate(key, self.read_at(key, ts))
        return capsule

    def commit(self, read_set: Dict[str, Optional[int]], writes: Dict[str, Optional[Document]]) -> int:
        for key, observed in read_set.items():
            live = self.read_at(key, self.now).version
            if live != observed:
                raise RuntimeError(f"conflict on {key}: observed={observed}, live={live}")
        self.now += 1
        commit_ts = self.now
        for key, doc in writes.items():
            self.docs.setdefault(key, []).append((commit_ts, None if doc is None else dict(doc)))
        return commit_ts


def run_sum(store: Mvcc, ts: int, keys: List[str], prewarm: List[str]) -> Tuple[int, int]:
    capsule = store.build_capsule(ts, prewarm)
    traps = 0
    while True:
        total = 0
        for key in keys:
            value = capsule.get(key)
            if value is None:
                traps += 1
                capsule.hydrate(key, store.read_at(key, ts))
                break
            if value.document is not None:
                total += int(value.document.get("value", 0))
        else:
            return total, traps


def run_increment(store: Mvcc, ts: int, key: str, prewarm: List[str]) -> Tuple[int, int, Dict[str, Optional[int]], Dict[str, Optional[Document]]]:
    capsule = store.build_capsule(ts, prewarm)
    traps = 0
    while True:
        value = capsule.get(key)
        if value is None:
            traps += 1
            capsule.hydrate(key, store.read_at(key, ts))
            continue
        current = 0 if value.document is None else int(value.document.get("value", 0))
        read_set = {key: value.version}
        writes = {key: {"value": current + 1}}
        return current + 1, traps, read_set, writes


def time_loop(iterations: int, op) -> Tuple[int, List[int]]:
    samples: List[int] = []
    start_total = time.perf_counter_ns()
    for _ in range(iterations):
        start = time.perf_counter_ns()
        op()
        samples.append(time.perf_counter_ns() - start)
    return time.perf_counter_ns() - start_total, samples


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--iterations", type=int, default=5000)
    parser.add_argument("--keys", type=int, default=32)
    args = parser.parse_args()

    store = Mvcc()
    keys = [f"items/{idx:04d}" for idx in range(args.keys)]
    for key in keys:
        store.seed(key, {"value": 1})
    store.seed("counter/main", {"value": 0})

    def warm_query() -> None:
        total, traps = run_sum(store, store.snapshot_ts(), keys, keys)
        assert total == args.keys
        assert traps == 0

    def cold_query() -> None:
        total, traps = run_sum(store, store.snapshot_ts(), keys, [])
        assert total == args.keys
        assert traps == args.keys

    def mutation() -> None:
        next_value, _traps, read_set, writes = run_increment(store, store.snapshot_ts(), "counter/main", [])
        assert next_value > 0
        store.commit(read_set, writes)

    results = {}
    for name, op in [
        ("warm_query", warm_query),
        ("cold_trap_query", cold_query),
        ("mutation", mutation),
    ]:
        total_ns, samples = time_loop(args.iterations, op)
        results[f"{name}_total_ns"] = total_ns
        results[f"{name}_avg_ns"] = total_ns // max(args.iterations, 1)
        results[f"{name}_p50_ns"] = int(statistics.median(samples))
        results[f"{name}_p95_ns"] = int(statistics.quantiles(samples, n=20)[18])

    results["iterations"] = args.iterations
    results["keys"] = args.keys
    print(json.dumps(results, indent=2, sort_keys=True))


if __name__ == "__main__":
    main()
