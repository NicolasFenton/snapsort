# Plan: adaptive concurrency

Make snapsort find the fastest safe number of parallel SubQ requests on its
own, so a fast connection runs fast and a slow one isn't overloaded. Today the
number is fixed by `-j` (default 32).

## Why

A rate-limit probe (`scripts/ratelimit_probe.py`, 528 real requests, about
$0.02) on a 40 Mbit/s upload connection measured:

| In flight | Photos/s | Median latency | p95 latency | 429s |
| ---: | ---: | ---: | ---: | ---: |
| 4 | 2.3 | 1.66 s | 2.19 s | 0 |
| 8 | 4.4 | 1.60 s | 2.66 s | 0 |
| 16 | 7.8 | 1.67 s | 2.69 s | 0 |
| 32 | 13.4 | 1.74 s | 3.61 s | 0 |
| 64 | 16.8 | 2.89 s | 6.36 s | 0 |

- SubQ never rate-limited. Throughput flattened at 64 because the upload link
  filled up: 16.8 requests/s × 226 KiB ≈ 31 Mbit/s, close to the measured
  39.6 Mbit/s uplink.
- The bottleneck showed up as **rising latency**, not as errors. Up to 32,
  latency stayed flat. At 64 the median almost doubled. That is the signal an
  adaptive controller needs.
- The right number depends on the connection, so no fixed default suits
  everyone. 32 leaves speed unused on fast links. 64 overloads slow ones:
  requests queue in the router, and other people on the network notice.

## What the research says

Adaptive concurrency is a solved problem, borrowed from TCP congestion control.
The concurrency limit plays the role of TCP's congestion window. Little's law
(throughput = concurrency ÷ latency) means that adding requests helps until
latency starts rising, and after that it only builds queues.

| Approach | Signal | How the limit moves | Notes |
| --- | --- | --- | --- |
| **AIMD** (Netflix `AIMDLimit`) | Errors and timeouts | +1 when at least half the limit is in use; × 0.9 on a drop or when RTT exceeds a timeout | Netflix recommends loss-based limits (or a mix of loss and delay) for clients |
| **Vegas** (Netflix `VegasLimit`) | Queue estimate `limit × (1 − minRTT / RTT)` | ±1 per window against thresholds (alpha 2–3, beta 4–6) | Needs a reliable minimum RTT |
| **Gradient2** (Netflix) | Long-term vs current RTT | `limit × clamp(1.5 × longRTT / RTT, 0.5, 1) + 4`, smoothed by 0.2 | Skips growth when less than half the limit is in use |
| **Envoy adaptive concurrency** | p90 RTT per 100 ms window vs a minimum RTT | Gradient | Re-measures minimum RTT every 60 s by briefly dropping concurrency. Too disruptive for a short CLI run |
| **Vector ARC** (Rust, production) | Mean RTT per window vs a running average (EWMA), plus back pressure | +1 per window; × 0.9 on back pressure or a latency jump | Closest fit, details below |
| **AWS SDK adaptive retry** | Throttling responses | CUBIC-style send-rate bucket | Limits request *rate*, not concurrency, and AWS labels it experimental |

How Vector's controller works (`src/sinks/util/adaptive_concurrency/controller.rs`):

- It measures response times in windows about one round trip long.
- At the end of each window it folds that window's average response time into
  a running average (an EWMA, `ewma_alpha` 0.4), and also tracks how much the
  times normally vary.
- **Increase by 1** only when all of these hold: the limit was actually
  reached during the window (so idle capacity can't inflate it), there was no
  back pressure, and the window's response time was at or below the running
  average.
- **Decrease** to `limit × decrease_ratio` (0.9), rounded down and at least 1,
  when there was back pressure, or when the response time is at or above the
  average plus 2.5 times the normal variation (`rtt_deviation_scale`).
- Back pressure means a retryable response (429 or 503) or a timeout. Other
  HTTP errors don't count.
- Only successful responses feed the response-time measurement.
- Defaults: starts at 1, never above 200 (`max_concurrency_limit`).

Existing Rust crates: `tower-acc` 0.1.1 (Aimd, Gradient2, Vegas; works only as
a Tower layer), `congestion-limiter` 0.1.0, and `flow-guard` (Vegas; an early
version had a semaphore bug). All three are 0.x releases from the last few
weeks, and snapsort calls reqwest directly rather than through Tower.

## Decisions

1. **Adjust during the run instead of testing the connection first.** A speed
   test would add about 20 s to every run, measure the wrong server, and
   couldn't tell which of upload speed, SubQ or the CPU is the bottleneck.
   Response times from the real requests measure all three, cost nothing
   extra, and keep working if the Wi-Fi changes halfway through.
2. **Use Vector's rules, plus a fast start.** They are proven in production in
   Rust and fit a client calling one API. Vector starts at 1 and adds 1 per
   window, which would take about 30 windows (around 50 s) to reach 32.
   That's most of a 1,000-photo run, so add a TCP-style *slow start*: double
   the limit each window until the first sign of congestion, then switch to
   Vector's +1 / × 0.9.
3. **Write it ourselves**, about 200 lines in `src/concurrency.rs`, rather
   than take on a new 0.x dependency. Reimplement from the description above;
   don't copy Vector's code, because Vector is MPL-2.0 and snapsort is MIT.
4. **Limit the HTTP requests only, not the whole per-photo pipeline.** Each
   attempt gets a slot just before it is sent and gives it back as soon as the
   response arrives. Slots are not held while waiting to retry. Previews keep
   being built ahead, up to the `-j` ceiling, so the network never waits on
   the CPU.
5. **Track the limit with a counter, not by resizing a tokio `Semaphore`.**
   `Semaphore::forget_permits` only removes permits that are currently free
   and returns how many it actually removed. Shrinking while requests are in
   flight would leave the semaphore allowing more than the limit. A
   `Mutex<{limit, in_flight, paused_until}>` with `tokio::sync::Notify` is
   exact and simple.

## Design

### Command line

- `-j, --concurrency <auto|N>`, default `auto`.
  - `auto` adapts between 1 and 128.
  - A number gives exactly that many, with no adapting. This is today's
    behaviour and is useful for scripts and comparisons.
- The help text and README explain both.

### Controller

Settings are named constants in `src/concurrency.rs`, not command-line flags:

| Setting | Value | Why |
| --- | --- | --- |
| Starting limit | 16 | Zero 429s and flat latency at 16 in the probe, and only about 4 MB in flight even on a slow link |
| Ceiling (auto) | 128 | Twice the highest level tested. 429 handling covers SubQ limits we haven't seen |
| Floor | 1 | |
| EWMA weight | 0.4 | Vector default |
| Variation scale | 2.5 | Vector default; Vector suggests 1.0–3.0 as reasonable |
| Decrease on slow responses | × 0.9 | Vector and Netflix default |
| Decrease on 429 or 503 | × 0.5 | An explicit "too many requests". The other requests in flight will likely be refused too, so back off harder. This is our choice, not taken from Vector, so tune it if needed |

Each **window** lasts the current average response time (about 1.7 s on the
test connection). The first window only records a baseline. After that, at the
end of each window:

```
congested = back_pressure || window_rtt >= mean + 2.5 * deviation

slow start:
    congested                      -> limit = max(limit / 2, 1); leave slow start
    reached_limit                  -> limit = min(limit * 2, ceiling)
avoidance:
    reached_limit && !congested
      && window_rtt <= mean        -> limit += 1
    back_pressure                  -> limit = max(limit * 0.5, 1)
    congested                      -> limit = max(limit * 0.9, 1)
then fold window_rtt into mean and deviation; reset the window
```

- `reached_limit` becomes true when a request takes a slot and the number in
  flight reaches the limit. This is the "app-limited" guard from Vector and
  Gradient2: the limit can't grow when snapsort isn't actually using it, for
  example near the end of a run or while previews are still being built.
- **Response time is measured per attempt.** It runs from just before
  `send()` until the full response body has arrived, and only 2xx responses
  count. It includes upload time, which is the point: a full upload link shows
  up as slower responses.
- **What counts as a signal:**

  | Response | Effect |
  | --- | --- |
  | 2xx | Response-time sample |
  | 429, 503, 504, reqwest timeout | Back pressure |
  | Other 5xx, 408, 409, 425 | Retried, but no signal |
  | 400, 401–405, 422 | No signal (not about load) |

- **Shared pause on `Retry-After`.** When a 429 or 503 carries
  `Retry-After`, set `paused_until` so *no* request starts before then.
  Without this, the other requests in flight hit the same 429. This is what
  AWS adaptive mode does by delaying even first attempts. The per-task retry
  delay stays as it is.

Expected behaviour on the test connection, based on the probe numbers: the
first window sets a baseline at 16, then the limit goes 16 → 32 → 64. At 64 the
median response time (2.9 s) is above the threshold (about 1.7 s average plus
2.5 times the variation), so it halves to 32, then climbs by 1 per window and
backs off by 10% around the point where uploads start queuing. That should
reach full speed within about 7 s.

### Where the code changes

| File | Change |
| --- | --- |
| `src/concurrency.rs` (new) | `Limiter { mode, state: Mutex<State>, notify: Notify }`. `acquire().await -> Slot`; `Slot::finish(Outcome::{Success(rtt), BackPressure { retry_after }, Neutral})`; dropping a `Slot` without calling `finish` counts as `Neutral`. Window, slow-start and EWMA logic sit in a pure `State::on_window_end(now)` that takes the time as a parameter, so it can be unit-tested |
| `src/cli.rs` | Parse `Concurrency::{Auto, Fixed(n)}` for `-j`. Update `parse_concurrency`, `AFTER_HELP` and the tests |
| `src/api.rs` | `classify` takes a slot before each attempt, finishes it with the outcome, and releases it before `retry_delay` sleeps. Map statuses and timeouts to outcomes |
| `src/app.rs` | Pipeline `permits` and `pool_max_idle_per_host` use the ceiling (128 for auto, N when fixed). `cpu_slots` clamps to the ceiling. Create the `Limiter` and pass it to `SubqClient` |
| `src/ui.rs` | `Tally` gains `limit: Option<usize>`, and the status line reads `in flight 31 / 32 (auto)` |
| `src/report.rs` | JSON report gains `"concurrency": { "mode", "final_limit", "peak_limit", "back_pressure_events", "pauses" }`. The summary prints one line when back pressure happened |
| `README.md` | Explain auto vs fixed, and update the paragraph about upload speed |

## Implementation steps

Each step builds, passes `cargo clippy -D warnings`, and passes the tests.

1. **`concurrency.rs` state machine with no async.** `State` plus a pure
   `on_window_end(now)`, with unit tests using made-up times and response
   times:
   - flat response times with the limit reached double it up to the ceiling;
   - a jump in response time ends slow start and halves the limit, then +1
     per window;
   - a 429 multiplies by 0.5 and sets the pause;
   - no growth while the limit is never reached;
   - the limit stays within [1, ceiling];
   - fixed mode never changes.
2. **The async gate.** `Limiter::acquire` and `Slot`, built on `Mutex` and
   `Notify`. Tests: shrinking the limit while slots are held never admits
   more than the limit; a pause holds back new slots until it ends; dropping
   a slot frees it.
3. **Connect it to `api.rs`.** Classify outcomes, measure each attempt's
   response time, release the slot during retry delays. The existing API
   tests keep passing in fixed mode.
4. **The `-j auto|N` option and `app.rs` plumbing.** Make `auto` the default.
5. **Progress line and JSON report fields.**
6. **End-to-end tests in `tests/cli.rs`**, using the existing mock server with
   two new behaviours:
   - a *bandwidth* mock where latency = 30 ms + 20 ms × max(0, in-flight − 12).
     Assert that every file sorts and the report's final limit is between 8
     and 24;
   - a *429* mock that rejects above 10 in flight with `Retry-After: 1`.
     Assert that every file sorts, at least one back-pressure event is
     recorded, and the final limit is at most 10.
7. **Real-world check** on the agency connection, with `--dry-run`, on the
   same set of about 500 real photos: `-j auto`, `-j 32` and `-j 64`, with
   the JSON report for each.
8. **README and help text.**

## How we'll know it worked

- `-j auto` gets at least 90% of the best fixed setting's photos per second on
  the test connection, and never runs slower than `-j 16`.
- It reaches 32 or more within 10 s on that connection.
- No more 429s or failed files than the best fixed setting.
- On the 429 mock, it settles at or below the mock's limit instead of looping
  between 429s and retries.
- All existing tests pass unchanged in fixed mode.

## Risks and open questions

- **Per-image variation in SubQ's processing time** could look like
  congestion. Window averages and the 2.5 × variation threshold should absorb
  it. If the limit wobbles in the real-world check, raise the scale toward 3.0
  or measure the median per window instead of the mean.
- **Short runs** of under about 200 photos finish before the limit settles.
  Starting at 16 keeps them close to today's speed. Remembering the last
  limit (below) would remove the gap entirely.
- **Shared office upload.** Adapting on response times keeps router queues
  short, which is kinder to colleagues' video calls than a fixed `-j 64`.
  Under load, the speed test rated upload responsiveness "Low" (677 ms).
- **SubQ limits above 64 in flight are unknown.** Handling 429s and pausing on
  `Retry-After` covers them, but the first 429s will cost some retries.
- **Open questions:**
  - Should `-j N` be a fixed value (recommended, as today) or a ceiling for
    auto?
  - Remember the last limit in `~/.config/snapsort/` and start from half of
    it next run? (Recommended, as a follow-up.)
  - Is a 128 ceiling right for gigabit offices?

## Sources

- [Netflix concurrency-limits](https://github.com/Netflix/concurrency-limits):
  README;
  [`AIMDLimit.java`](https://github.com/Netflix/concurrency-limits/blob/master/concurrency-limits-core/src/main/java/com/netflix/concurrency/limits/limit/AIMDLimit.java);
  [`Gradient2Limit.java`](https://github.com/Netflix/concurrency-limits/blob/master/concurrency-limits-core/src/main/java/com/netflix/concurrency/limits/limit/Gradient2Limit.java)
- Vector ARC:
  [announcement](https://vector.dev/blog/adaptive-request-concurrency/),
  [`controller.rs`](https://github.com/vectordotdev/vector/blob/master/src/sinks/util/adaptive_concurrency/controller.rs),
  [`request.adaptive_concurrency` options](https://vector.dev/docs/reference/configuration/sinks/http/),
  [Greptime write-up](https://greptime.com/blogs/2025-01-09-vector-adaptive-request-concurrency)
- [Envoy adaptive concurrency filter](https://www.envoyproxy.io/docs/envoy/latest/configuration/http/http_filters/adaptive_concurrency_filter)
- [AWS SDKs: retry behavior and adaptive mode](https://docs.aws.amazon.com/general/latest/gr/api-retries.html)
- [tokio `Semaphore`](https://docs.rs/tokio/latest/tokio/sync/struct.Semaphore.html)
  (`add_permits`, `forget_permits`)
- Rust crates considered: [tower-acc](https://docs.rs/tower-acc),
  [congestion-limiter](https://docs.rs/congestion-limiter),
  [FlowGuard](https://users.rust-lang.org/t/announcing-flowguard-next-gen-adaptive-backpressure-for-the-rust-ecosystem/137084)
- [SubQ Decisions API](https://docs.subq.ai/endpoints/decisions-api/),
  [models and pricing](https://docs.subq.ai/using-the-api/models/)
