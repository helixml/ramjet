#!/usr/bin/env python3
"""Summarize vLLM KV replay integrity without printing tokens or hashes.

Run inside the vLLM environment that provides pyzmq and msgspec:
  kv_event_replay_probe.py tcp://127.0.0.1:5558 [start_sequence]

External hashes are retained only in process memory to verify parent ordering.
Output contains bounded counts and sequence positions, never raw identifiers.
"""

import collections
import json
import sys

import msgspec
import zmq


ENDPOINT = sys.argv[1] if len(sys.argv) > 1 else "tcp://127.0.0.1:5558"
START = int(sys.argv[2]) if len(sys.argv) > 2 else 0
MAIN_KINDS = {"full_attention", "mla_attention", "sink_full_attention"}

context = zmq.Context.instance()
socket = context.socket(zmq.DEALER)
socket.setsockopt(zmq.RCVTIMEO, 10_000)
socket.connect(ENDPOINT)
socket.send_multipart([b"", START.to_bytes(8, "big")])

sequences = []
event_counts = collections.Counter()
main_shapes = collections.Counter()
seen_any = set()
seen_main = set()
seen_indexable_main = set()
missing_main_parents = []
main_parent_seen_any_only = 0
missing_main_parent_details = collections.Counter()
removed_hashes = 0
removed_by_group = collections.Counter()

while True:
    frames = socket.recv_multipart()
    if len(frames) != 4:
        raise RuntimeError("unexpected replay frame count")
    delimiter, topic, sequence_bytes, payload = frames
    if delimiter != b"":
        raise RuntimeError("unexpected replay delimiter")
    if sequence_bytes == b"\xff" * 8:
        break
    sequence = int.from_bytes(sequence_bytes, "big")
    sequences.append(sequence)
    batch = msgspec.msgpack.decode(payload)
    for event in batch[1]:
        kind = event.get("type", "unknown")
        event_counts[kind] += 1
        if kind == "BlockRemoved":
            removed = len(event.get("block_hashes") or [])
            removed_hashes += removed
            removed_by_group[f"group={event.get('group_idx', 'unknown')}"] += removed
            continue
        if kind != "BlockStored":
            continue
        hashes = event.get("block_hashes") or []
        parent = event.get("parent_block_hash")
        attention = event.get("kv_cache_spec_kind", "unknown")
        if attention in MAIN_KINDS:
            has_extra_keys = any(
                value is not None for value in (event.get("extra_keys") or [])
            )
            indexable = event.get("lora_name") is None and not has_extra_keys
            token_count = len(event.get("token_ids") or [])
            hash_count = len(hashes)
            shape = "exact"
            block_size = event.get("block_size") or 0
            if block_size == 0 or (token_count + block_size - 1) // block_size != hash_count:
                shape = "mismatch"
            parent_state = "root"
            if parent is not None:
                parent_state = (
                    "seen_indexable"
                    if parent in seen_indexable_main
                    else "seen_filtered_main"
                    if parent in seen_main
                    else "missing_main"
                )
                if parent_state == "missing_main":
                    missing_main_parents.append(sequence)
                    main_parent_seen_any_only += parent in seen_any
                    missing_main_parent_details[
                        f"sequence={sequence}:group={event.get('group_idx', 'unknown')}:"
                        f"block={event.get('block_size', 'unknown')}:hashes={len(hashes)}:"
                        f"tokens={len(event.get('token_ids') or [])}:"
                        f"extra_keys={'yes' if has_extra_keys else 'no'}"
                    ] += 1
            main_shapes[
                f"{attention}:block={block_size}:shape={shape}:parent={parent_state}"
            ] += 1
            seen_main.update(hashes)
            if indexable:
                seen_indexable_main.update(hashes)
        seen_any.update(hashes)

gaps = sum(
    max(0, current - previous - 1)
    for previous, current in zip(sequences, sequences[1:])
)
print(
    json.dumps(
        {
            "endpoint": ENDPOINT,
            "requested_start": START,
            "batches": len(sequences),
            "first_sequence": sequences[0] if sequences else None,
            "last_sequence": sequences[-1] if sequences else None,
            "sequence_gaps": gaps,
            "events": dict(sorted(event_counts.items())),
            "main_shapes": dict(sorted(main_shapes.items())),
            "main_hashes_seen": len(seen_main),
            "indexable_main_hashes_seen": len(seen_indexable_main),
            "all_hashes_seen": len(seen_any),
            "removed_hashes": removed_hashes,
            "removed_by_group": dict(sorted(removed_by_group.items())),
            "missing_main_parent_events": len(missing_main_parents),
            "missing_parent_seen_only_in_non_main": main_parent_seen_any_only,
            "missing_main_parent_details": dict(
                sorted(missing_main_parent_details.items())
            ),
            "first_missing_parent_sequence": (
                missing_main_parents[0] if missing_main_parents else None
            ),
        },
        sort_keys=True,
    )
)
