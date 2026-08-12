#!/usr/bin/env python3
"""Summarize vLLM KV-event traffic without printing token IDs or hashes.

Run inside a vLLM environment that provides pyzmq and msgspec:
  kv_event_probe.py tcp://127.0.0.1:5557 [seconds]

The probe intentionally keeps payloads in memory only and emits aggregate
counts. BlockStored events contain exact token IDs, so raw payloads must not be
logged or copied out of the trusted serving network.
"""

import collections
import json
import sys
import time

import msgspec
import zmq


ENDPOINT = sys.argv[1] if len(sys.argv) > 1 else "tcp://127.0.0.1:5557"
SECONDS = float(sys.argv[2]) if len(sys.argv) > 2 else 30.0


def event_type(event):
    if isinstance(event, dict):
        return event.get("type") or event.get("tag") or "unknown"
    return "unknown"


context = zmq.Context.instance()
socket = context.socket(zmq.SUB)
socket.setsockopt(zmq.SUBSCRIBE, b"")
socket.connect(ENDPOINT)
poller = zmq.Poller()
poller.register(socket, zmq.POLLIN)

deadline = time.monotonic() + SECONDS
sequences = []
counts = collections.Counter()
stored_blocks = stored_tokens = removed_blocks = 0
block_sizes = collections.Counter()
data_parallel_ranks = collections.Counter()

while time.monotonic() < deadline:
    remaining_ms = max(1, int((deadline - time.monotonic()) * 1000))
    if socket not in dict(poller.poll(min(remaining_ms, 1000))):
        continue
    frames = socket.recv_multipart()
    if len(frames) != 3:
        counts["malformed_batch"] += 1
        continue
    _, sequence_bytes, payload = frames
    sequences.append(int.from_bytes(sequence_bytes, "big"))
    batch = msgspec.msgpack.decode(payload)
    if not isinstance(batch, list) or len(batch) < 2:
        counts["malformed_payload"] += 1
        continue
    if len(batch) > 2 and batch[2] is not None:
        data_parallel_ranks[str(batch[2])] += 1
    for event in batch[1]:
        kind = event_type(event)
        counts[kind] += 1
        if kind == "BlockStored":
            hashes = event.get("block_hashes") or []
            token_ids = event.get("token_ids") or []
            stored_blocks += len(hashes)
            stored_tokens += len(token_ids)
            if event.get("block_size") is not None:
                block_sizes[str(event["block_size"])] += len(hashes)
        elif kind == "BlockRemoved":
            removed_blocks += len(event.get("block_hashes") or [])

gaps = 0
for previous, current in zip(sequences, sequences[1:]):
    gaps += max(0, current - previous - 1)

print(
    json.dumps(
        {
            "endpoint": ENDPOINT,
            "seconds": SECONDS,
            "batches": len(sequences),
            "first_sequence": sequences[0] if sequences else None,
            "last_sequence": sequences[-1] if sequences else None,
            "sequence_gaps": gaps,
            "events": dict(sorted(counts.items())),
            "stored_blocks": stored_blocks,
            "stored_token_ids": stored_tokens,
            "removed_blocks": removed_blocks,
            "block_sizes": dict(sorted(block_sizes.items())),
            "data_parallel_ranks": dict(sorted(data_parallel_ranks.items())),
        },
        sort_keys=True,
    )
)
