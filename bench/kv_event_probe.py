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
event_keys = collections.Counter()
event_value_types = collections.Counter()
stored_blocks = stored_tokens = removed_blocks = 0
block_sizes = collections.Counter()
stored_shape_mismatches = 0
stored_shapes = collections.Counter()
stored_extra_key_shapes = collections.Counter()
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
        if isinstance(event, dict):
            event_keys[f"{kind}:{','.join(sorted(event))}"] += 1
            for key, value in event.items():
                event_value_types[f"{kind}.{key}:{type(value).__name__}"] += 1
        if kind == "BlockStored":
            hashes = event.get("block_hashes") or []
            token_ids = event.get("token_ids") or []
            stored_blocks += len(hashes)
            stored_tokens += len(token_ids)
            if event.get("block_size") is not None:
                block_size = event["block_size"]
                block_sizes[str(block_size)] += len(hashes)
                expected_blocks = (len(token_ids) + block_size - 1) // block_size
                mismatch = expected_blocks != len(hashes)
                stored_shape_mismatches += mismatch
                stored_shapes[
                    f"{event.get('kv_cache_spec_kind', 'unknown')}:"
                    f"group={event.get('group_idx', 'unknown')}:"
                    f"block={block_size}:"
                    f"{'mismatch' if mismatch else 'exact'}"
                ] += 1
            for extra_keys in event.get("extra_keys") or []:
                if extra_keys is None:
                    stored_extra_key_shapes["none"] += 1
                elif isinstance(extra_keys, list):
                    stored_extra_key_shapes[f"list:{len(extra_keys)}"] += 1
                else:
                    stored_extra_key_shapes[type(extra_keys).__name__] += 1
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
            "event_keys": dict(sorted(event_keys.items())),
            "event_value_types": dict(sorted(event_value_types.items())),
            "stored_blocks": stored_blocks,
            "stored_token_ids": stored_tokens,
            "stored_shape_mismatches": stored_shape_mismatches,
            "stored_shapes": dict(sorted(stored_shapes.items())),
            "stored_extra_key_shapes": dict(sorted(stored_extra_key_shapes.items())),
            "removed_blocks": removed_blocks,
            "block_sizes": dict(sorted(block_sizes.items())),
            "data_parallel_ranks": dict(sorted(data_parallel_ranks.items())),
        },
        sort_keys=True,
    )
)
