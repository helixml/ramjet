# Implement a token-bucket rate limiter

We need a rate limiter for outgoing API calls. `ratelimit/bucket.py` contains
a stub of `TokenBucket`; please implement it to this spec.

## Constructor

`TokenBucket(capacity, refill_rate, clock=time.monotonic)`

- `capacity`: maximum number of tokens; must be `> 0` (int or float).
- `refill_rate`: tokens added per second; must be `>= 0`.
- `clock`: a zero-argument callable returning the current time in seconds
  (float). Tests inject a fake clock. It is called on construction and on
  every operation below.
- Invalid `capacity` or `refill_rate` raises `ValueError`.
- A new bucket starts **full**.

## Refill

Tokens accumulate continuously at `refill_rate` per second of elapsed clock
time, never exceeding `capacity`. Fractional tokens are kept. If the clock
ever goes backwards, treat that step as zero elapsed time (tokens never
decrease because of the clock) and measure further refill from the new,
earlier reading.

## `allow(n=1) -> bool`

- `n` must be `> 0`, otherwise `ValueError`.
- If at least `n` tokens are available, consume `n` and return `True`.
- Otherwise consume nothing and return `False` (all or nothing; a denied
  request must not reduce the tokens available to a later, smaller request).
- If `n > capacity` the request can never succeed: return `False`.

## `tokens` (read-only property)

The number of tokens currently available (after refill), as a float.

## `retry_after(n=1) -> float`

Seconds from now until `allow(n)` would succeed if nothing else consumed
tokens: `0.0` if it would succeed now, `math.inf` if it never can (`n >
capacity`, or not enough tokens and `refill_rate == 0`). Does not consume
tokens. `n <= 0` raises `ValueError`.
