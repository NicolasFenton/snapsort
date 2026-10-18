#!/usr/bin/env python3
"""Find where the SubQ Decisions API starts rate-limiting.

Sends the same request snapsort sends (one preview, the default categories) at
rising concurrency levels, with no retries, and reports per level: throughput,
429s, latency, and any rate-limit headers SubQ returns. Stops escalating once a
level sees more than --max-429 of its requests rejected.

    python3 scripts/ratelimit_probe.py [PHOTO] [--levels 4,8,16,32,64]

The key comes from SUBQ_API_KEY, else .env.local in the repo root. It is never
printed. Without PHOTO a synthetic image is used, so no real photos leave the
machine. Needs Pillow.
"""

import argparse
import base64
import io
import json
import os
import statistics
import sys
import threading
import time
import urllib.error
import urllib.request
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

from PIL import Image, ImageDraw

REPO = Path(__file__).resolve().parent.parent
DEFAULT_URL = "https://api.subq.ai/v1/systemone"
DEFAULT_MODEL = "subq-snap-preview"
INSTRUCTIONS = "Select the single best folder category for this image."
FATAL = {401, 402, 403}
USD_PER_MILLION_TOKENS = 0.05  # subq-snap-preview input price, docs.subq.ai models page


def load_key():
    key = os.environ.get("SUBQ_API_KEY", "").strip()
    env_file = REPO / ".env.local"
    if not key and env_file.is_file():
        for line in env_file.read_text().splitlines():
            name, _, value = line.partition("=")
            if name.strip() == "SUBQ_API_KEY":
                key = value.strip().strip("'\"")
    if not key:
        sys.exit("No key: set SUBQ_API_KEY or add it to .env.local")
    return key


def preview_base64(photo, max_dim, quality):
    if photo:
        image = Image.open(photo).convert("RGB")
    else:
        # Busy enough to compress like a photo, not like a flat test card.
        image = Image.new("RGB", (1600, 1067), (96, 128, 64))
        draw = ImageDraw.Draw(image)
        for i in range(0, 1600, 8):
            draw.line([(i, 0), (1600 - i, 1067)], fill=(i % 256, (i * 3) % 256, 90))
    image.thumbnail((max_dim, max_dim))
    buffer = io.BytesIO()
    image.save(buffer, "JPEG", quality=quality)
    return base64.b64encode(buffer.getvalue()).decode()


def interesting_headers(headers):
    return {
        name.lower(): value
        for name, value in headers.items()
        if any(word in name.lower() for word in ("limit", "retry", "quota", "remaining", "reset"))
    }


class Probe:
    def __init__(self, url, key, body, timeout):
        self.url = url
        self.key = key
        self.body = body
        self.timeout = timeout
        self.headers_seen = {}
        self.tokens = 0
        self.lock = threading.Lock()

    def send(self):
        request = urllib.request.Request(
            self.url,
            data=self.body,
            method="POST",
            headers={"Authorization": f"Bearer {self.key}", "Content-Type": "application/json"},
        )
        started = time.monotonic()
        try:
            with urllib.request.urlopen(request, timeout=self.timeout) as response:
                payload = response.read()
                status, headers = response.status, response.headers
            try:
                tokens = json.loads(payload)["usage"]["input_tokens"]
            except (ValueError, KeyError, TypeError):
                tokens = 0
        except urllib.error.HTTPError as err:
            err.read()
            status, headers, tokens = err.code, err.headers, 0
        except Exception as err:  # network errors and timeouts
            return {"status": type(err).__name__, "latency": time.monotonic() - started}
        found = interesting_headers(headers)
        with self.lock:
            self.tokens += tokens
            if found:
                self.headers_seen.setdefault(status, found)
        return {"status": status, "latency": time.monotonic() - started}

    def level(self, concurrency, count):
        started = time.monotonic()
        with ThreadPoolExecutor(max_workers=concurrency) as pool:
            results = list(pool.map(lambda _: self.send(), range(count)))
        return results, time.monotonic() - started


def percentile(values, fraction):
    if not values:
        return float("nan")
    ordered = sorted(values)
    return ordered[min(len(ordered) - 1, int(fraction * len(ordered)))]


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("photo", nargs="?", help="photo to send (default: synthetic image)")
    parser.add_argument("--levels", default="4,8,16,32,64", help="concurrency levels to try")
    parser.add_argument("--requests", type=int, default=4, help="requests per level = this x concurrency (min 40)")
    parser.add_argument("--cooldown", type=float, default=15, help="seconds to wait between levels")
    parser.add_argument("--max-429", type=float, default=0.10, help="stop after a level with this share of 429s")
    parser.add_argument("--categories", default=str(REPO / "examples/categories.json"))
    parser.add_argument("--max-dim", type=int, default=1280)
    parser.add_argument("--quality", type=int, default=80)
    parser.add_argument("--timeout", type=float, default=120)
    parser.add_argument("--yes", action="store_true", help="skip the confirmation prompt")
    args = parser.parse_args()

    url = os.environ.get("SUBQ_API_URL", DEFAULT_URL)
    model = os.environ.get("SUBQ_MODEL", DEFAULT_MODEL)
    levels = [int(n) for n in args.levels.split(",")]
    counts = [max(40, args.requests * c) for c in levels]
    categories = json.loads(Path(args.categories).read_text())
    image = preview_base64(args.photo, args.max_dim, args.quality)
    body = json.dumps(
        {
            "model": model,
            "state": "Classify file: probe.jpg",
            "images": [{"mediaType": "image/jpeg", "base64": image}],
            "questions": {
                "target_folder": {"type": "choice", "instructions": INSTRUCTIONS, "criteria": categories}
            },
        }
    ).encode()

    print(f"endpoint  {url}  model {model}")
    print(f"preview   {len(body) / 1024:.0f} KiB per request, {len(categories)} categories")
    print(f"plan      levels {levels}, up to {sum(counts)} requests, no retries")
    if not args.yes and input("Send these requests? [y/N] ").strip().lower() not in ("y", "yes"):
        sys.exit("Cancelled; nothing was sent.")

    probe = Probe(url, load_key(), body, args.timeout)
    print(f"\n{'conc':>5} {'sent':>5} {'ok':>5} {'429':>5} {'other':>6} {'ok/s':>7} {'p50 s':>6} {'p95 s':>6}")
    # Report what was spent and seen even when a level aborts or on Ctrl-C.
    try:
        run_levels(probe, args, levels, counts)
    finally:
        cost = probe.tokens * USD_PER_MILLION_TOKENS / 1e6
        print(f"\ninput tokens billed: {probe.tokens:,} (about ${cost:.4f})")
        print("rate-limit headers seen:")
        if not probe.headers_seen:
            print("  none")
        for status, headers in sorted(probe.headers_seen.items(), key=lambda item: str(item[0])):
            for name, value in sorted(headers.items()):
                print(f"  [{status}] {name}: {value}")


def run_levels(probe, args, levels, counts):
    for index, (concurrency, count) in enumerate(zip(levels, counts)):
        if index:
            time.sleep(args.cooldown)
        results, elapsed = probe.level(concurrency, count)
        ok = [r for r in results if r["status"] == 200]
        limited = sum(1 for r in results if r["status"] == 429)
        other = [r["status"] for r in results if r["status"] not in (200, 429)]
        latencies = [r["latency"] for r in ok]
        print(
            f"{concurrency:>5} {count:>5} {len(ok):>5} {limited:>5} {len(other):>6} "
            f"{len(ok) / elapsed:>7.1f} {percentile(latencies, 0.5):>6.2f} {percentile(latencies, 0.95):>6.2f}"
        )
        if other:
            print(f"      other statuses: {sorted(set(map(str, other)))}")
        if FATAL & set(other):
            sys.exit("Stopping: SubQ rejected the key, credits or model (401/402/403).")
        if not ok and not limited:
            sys.exit("Stopping: every request failed without a rate limit; check the endpoint.")
        if limited / count > args.max_429:
            print(f"      over {args.max_429:.0%} rate-limited; not going higher.")
            return


if __name__ == "__main__":
    main()
