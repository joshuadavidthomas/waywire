#!/usr/bin/env python3
"""Summarize sender-saved JSON, retaining block/host variance and failures."""
import collections
import json
import math
import statistics
import sys


def distribution(values):
    values = sorted(values)
    if not values:
        return None
    return {"n": len(values), "p50": statistics.median(values), "p95": values[math.ceil(.95 * len(values)) - 1], "min": values[0], "max": values[-1]}


data = json.load(open(sys.argv[1]))
groups = collections.defaultdict(list)
blocks = []
for index, block in enumerate(data["blocks"]):
    if "measurementEndedAtMs" not in block:
        continue  # The delayed/stale correctness check is not a performance sample.
    config = block["configuration"]
    responses = [r for r in block["responses"] if not r.get("resync")]
    success = [r for r in responses if r["ok"]]
    frames = block["frames"]
    duration = (block["measurementEndedAtMs"] - block["measurementStartedAtMs"]) / 1000
    start, end = block["startStats"], block["endStats"]
    actual = {m["generation"]: m for m in block["metadata"]}
    settings = sorted({(f["generation"], f["width"], f["height"], actual[f["generation"]]["fps"], actual[f["generation"]]["chroma"], f["bitrateKbps"], f["scalePercent"], f["latencyTargetMs"]) for f in frames})
    key = f'{config["fps"]}/{config["mode"]}/reads{1 + block.get("extraReads", 0)}'
    playout = config.get("playout", {"mode": "adaptive"})
    if data.get("playout", {}).get("mode") == "per-block":
        key += f'/target{playout.get("targetMs", "adaptive")}'
    if config.get("videoJitter"):
        key += "/video-jitter"
    groups[key].extend(responses)
    blocks.append({
        "index": index, "condition": key, "senderSerial": config["serial"],
        "playout": config.get("playout", {"mode": "adaptive"}),
        "durationSeconds": duration, "success": len(success), "failures": len(responses) - len(success),
        "latencyMs": distribution([r["latencyMs"] for r in success]),
        "ackMs": distribution([r["ackAtMs"] - r["startedAtMs"] for r in success if r["ackAtMs"] is not None]),
        "rttMs": distribution([r["stats"]["rttMs"] for r in success]),
        "confidentLatenessMs": distribution([f["latenessMs"] for f in frames if f["clockConfident"]]),
        "rafIntervalMs": distribution(block["rafIntervals"]),
        "readbackMs": distribution([f["readbackMs"] for f in frames]),
        "settingsFields": ["generation", "width", "height", "fps", "chroma", "bitrateKbps", "scalePercent", "latencyTargetMs"],
        "settings": settings,
        "metadataTimeline": [{**m, "relativeMs": m["atMs"] - block["measurementStartedAtMs"]} for m in block["metadata"] if m["atMs"] <= block["measurementEndedAtMs"]],
        "qualityTimeline": [{**q, "relativeMs": q["atMs"] - block["measurementStartedAtMs"]} for q in block["quality"] if q["atMs"] <= block["measurementEndedAtMs"]],
        "clockSamplesTimeline": [{**c, "relativeMs": c["atMs"] - block["measurementStartedAtMs"]} for c in block.get("clockSamples", []) if c["atMs"] <= block["measurementEndedAtMs"]],
        "achievedRates": {name: (end[name] - start[name]) / duration for name in ["receivedFrames", "decodedFrames", "presentedFrames"]},
        "drops": {name: end[name] - start[name] for name in ["droppedFrames", "overdueDroppedFrames", "decodedOverflowDroppedFrames", "decoderResetDroppedFrames", "decoderResets"]},
        "capacityAndResetLoss": sum(end[name] - start[name] for name in ["decodedOverflowDroppedFrames", "decoderResetDroppedFrames"]),
        "latencyTargetMs": distribution([f["latencyTargetMs"] for f in frames]),
        "queue": distribution([f["decoderQueue"] for f in frames]),
        "presentationQueue": distribution([f["pendingVideoFrames"] for f in frames if "pendingVideoFrames" in f]),
        "clockUncertaintyMs": distribution([f["clockUncertaintyMs"] for f in frames if f["clockUncertaintyMs"] is not None]),
        "clockConfidentFraction": sum(f["clockConfident"] for f in frames) / len(frames) if frames else None,
    })
summary = {key: {"latencyMs": distribution([r["latencyMs"] for r in responses if r["ok"]]), "timeouts": sum(not r["ok"] for r in responses)} for key, responses in sorted(groups.items())}
print(json.dumps({"userAgent": data["userAgent"], "hardwareConcurrency": data["hardwareConcurrency"], "playout": data.get("playout", {"mode": "adaptive"}), "summary": summary, "blocks": blocks}, indent=2))
