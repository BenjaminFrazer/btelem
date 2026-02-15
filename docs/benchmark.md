# BTELEM_LOG Benchmark Results

Measured on x86_64 Linux, GCC 13.3, `-O2` (Release build).
Entry size: 256 bytes, ring: 1024 entries.

## Single-thread throughput by payload size

| Payload | ns/entry | M entries/s |
|---------|----------|-------------|
| 4B      | 27       | 36.7        |
| 16B     | 28       | 35.1        |
| 232B    | 37       | 26.6        |

Payload size has minimal impact until max — the 4B to 232B spread is only
10ns, meaning the `memcpy` of the entry struct is well-optimised.

## Multi-thread scaling (16B payload)

| Threads | ns/entry/thread | Aggregate M entries/s |
|---------|-----------------|----------------------|
| 1       | 28              | 33.9                 |
| 2       | 63              | 30.2                 |
| 4       | 135             | 26.5                 |
| 8       | 269             | 24.4                 |

Per-thread cost scales linearly with contention on the `fetch_add` cache
line. Aggregate throughput stays above 24M entries/s even at 8 threads.

## Cost breakdown (single-thread, 16B payload)

| Component                        | Approx cost |
|----------------------------------|-------------|
| `clock_gettime(CLOCK_MONOTONIC)` | ~18ns       |
| `fetch_add` (head)               | ~3ns        |
| `store_rel` (seq = 0)            | ~1ns        |
| Struct field writes + `memcpy`   | ~4ns        |
| `store_rel` (seq = committed)    | ~1ns        |
| **Total**                        | **~28ns**   |

The timestamp dominates at ~65% of the call-site cost.

## Alternative timestamp sources

| Method                  | Cost  | Resolution | Portable       |
|-------------------------|-------|------------|----------------|
| `CLOCK_MONOTONIC`       | 18ns  | ~1ns       | POSIX          |
| `CLOCK_MONOTONIC_COARSE`| 6ns   | ~1-4ms     | Linux          |
| `rdtsc` (x86)           | 7ns   | sub-ns     | x86 only       |
| `cntvct_el0` (ARM64)    | ~5ns  | ~20-40ns   | ARM64 only     |
| `rdtscp` (serialising)  | 11ns  | sub-ns     | x86 only       |

### Hardware counter analysis

Using `rdtsc`/`cntvct_el0` saves ~10ns per entry (28ns down to 20ns).
However:

- **Values are counter ticks, not nanoseconds.** The decoder must know the
  counter frequency to convert to wall time.
- **Frequency source is imprecise.** On x86, `sysfs` reports the nominal
  base frequency which can differ from actual TSC frequency by up to
  ~4000ppm (~0.4%). On ARM64, `cntfrq_el0` is set by firmware and is
  typically exact.
- **Pairwise accuracy is excellent.** Consecutive timestamp deltas converted
  via the reported frequency match `CLOCK_MONOTONIC` deltas to within
  ~400ns (jitter in the gap between the two clock reads).
- **Multi-thread scaling is unchanged.** At 2+ threads, `fetch_add`
  contention (60-280ns) dominates, making the timestamp source irrelevant.

### Conclusion

The 10ns saving (~30%) is meaningful in microbenchmarks but unlikely to
matter in practice. `CLOCK_MONOTONIC` provides nanosecond values directly
with no conversion, no platform-specific code, and no frequency
calibration. The `BTELEM_TIMESTAMP()` macro is overridable if a faster
source is needed on a specific target.

## Reproducing

```
make bench
```

Builds with `-O2` (Release) in a separate `build-bench/` directory.
