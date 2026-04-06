# Transport Tuning Low-Level Design

**Based on frontier simulator benchmarks with pcap validation**
**Date:** 2026-04-06
**Applies to:** NEXIO Android TV — `TransportPolicyController`, `DualLaneScheduler`, `ParallelRangeDataSource`

---

## 1. Executive Summary

The frontier simulator discovered that Real-Debrid and Premiumize have fundamentally different transport behaviors. Real-Debrid forces `Connection: close` after every range request and rate-limits at ~10 new TLS connections/second. Premiumize supports full HTTP keep-alive with a single TCP connection serving 200+ range requests. Using the same transport policy for both providers leaves 60-80% of available throughput on the table.

This document provides wire-level evidence for each finding and concrete code changes for NEXIO's transport stack.

---

## 2. Provider Transport Profiles (Empirical)

### 2.1 Real-Debrid

| Metric | Measured Value | Evidence |
|--------|---------------|----------|
| Keep-alive | **No** — server sends FIN after every response | pcap: every TCP stream has exactly 1 HTTP transaction, closed by FIN from CDN IP |
| Requests per connection | **1** (server-enforced) | pcap stream analysis: ~4.2 MB per stream (4MB chunk + HTTP/TLS overhead) |
| TLS handshake cost | 40-96 ms per connection | trace: `connect_ms` 18-32ms + `tls_ms` 40-96ms |
| Connection rate limit | ~8-10 new TLS connections/second sustained | pcap: TLS Client Hello rate stable at 6-10/sec, cutoff triggered at 70+/sec |
| Connection cutoff threshold | ~300 total connections in a rolling window | trace: cutoff at 38-41s with 115+ connections opened, RST storm follows |
| RST behavior | Server (CDN IP) sends RST to all new connections during rate limit | pcap: **1285 RSTs, all from 162.19.10.28, zero from client** |
| Rate limit recovery | ~15-20 seconds of backoff before CDN accepts new connections | trace timeline: RST storm from 39s-56s, recovery at 56-57s |
| Max throughput (4MB chunks, 3 workers) | 183-249 Mbps | Constrained by connection overhead and rate limiting |
| Max throughput (16MB chunks, 3 workers) | 571 Mbps, zero retries | Fewer connections = under rate limit |
| Max throughput (32MB chunks, 2 workers) | **604 Mbps**, zero stalls, 0.0 stability penalty | 68 connections in 60s — well under limit |
| HTTP version | HTTP/1.1 exclusively | pcap + trace: all successful requests report h1.1 |

**Root cause chain:**
```
Small chunks (4MB) → many requests → new TCP+TLS per request (server closes)
→ high connection rate → CDN rate limit at ~300 total connections
→ RST storm → all new connections rejected → frontier freeze → rebuffer
```

### 2.2 Premiumize

| Metric | Measured Value | Evidence |
|--------|---------------|----------|
| Keep-alive | **Yes** — single TCP connection serves 200+ requests | trace: 231 requests across 1 connection |
| Requests per connection | **Unlimited** (observed 235 on single conn) | trace: `connections: 1` across all worker configs |
| TLS handshake cost | One-time ~60ms at connection start | Only 1 connection opened per worker |
| Connection rate limit | Not observed | Zero retries, zero failures in any configuration |
| Optimal config | **16MB chunks × 3 workers** | Highest throughput + safe budget with zero stalls |
| Max throughput | **856 Mbps** (16MB × 3w) | 384 requests, 1 connection, 13,779 frontier events |
| Max safe budget | **762 Mbps** (16MB × 3w) | Requires sufficient frontier event resolution |
| Why not 24-32MB? | Safe budget underestimated at 425 Mbps | Fewer frontier events = coarser binary search resolution |
| Why not 4 workers? | 2.1s stall with 4 workers | Single TCP connection can't feed 4 parallel readers |
| HTTP version | HTTP/1.1 | CDN: energycdn.com |

**Root cause chain:**
```
Keep-alive → single connection → zero TLS overhead after first request
→ no rate limit concern → chunk size irrelevant for connection health
→ pure bandwidth-limited throughput
```

---

## 3. Optimal Transport Policies

### 3.1 Real-Debrid Policy

```kotlin
// TransportPolicyController — Real-Debrid profile
val REALDEBRID_POLICY = TransportPolicy(
    maxSafeUrgentWorkers = 2,
    maxSafePrefetchWorkers = 1,
    // Large chunks minimize connections (1 conn per request, server-enforced)
    maxSafeUrgentChunkBytes = 16L * 1024 * 1024,   // 16 MB
    maxSafePrefetchChunkBytes = 32L * 1024 * 1024,  // 32 MB
    // Connection rate budget: max 8 new connections/second
    // With 16MB chunks at ~300 Mbps: ~2.4 requests/sec = 2.4 conns/sec (safe)
    maxNewConnectionsPerSecond = 8,
    // No keep-alive — expect Connection: close on every response
    expectKeepAlive = false,
)
```

**Why 16MB for urgent, 32MB for prefetch:**
- At 300 Mbps: 16MB chunk = ~0.43s transfer + 0.06s TLS = ~2 requests/sec/worker
- 2 urgent workers = ~4 connections/second — well under the 8-10/sec limit
- Prefetch at 32MB = ~0.85s transfer — even lower connection rate
- Total: ~5 connections/second → 300 in 60s → stays under the ~300 connection budget

### 3.2 Premiumize Policy

```kotlin
// TransportPolicyController — Premiumize profile
val PREMIUMIZE_POLICY = TransportPolicy(
    maxSafeUrgentWorkers = 3,
    maxSafePrefetchWorkers = 1,
    // 16 MB is the sweet spot: enough frontier granularity for accurate
    // safe budget measurement, small enough for fast frontier advancement
    maxSafeUrgentChunkBytes = 16L * 1024 * 1024,   // 16 MB
    maxSafePrefetchChunkBytes = 32L * 1024 * 1024,  // 32 MB
    maxNewConnectionsPerSecond = 0, // No limit needed
    expectKeepAlive = true,
)
```

**Why 16MB for urgent on Premiumize:**
- Connection reuse means zero TLS overhead per request (single keep-alive connection)
- 16MB chunks produce ~13,000+ frontier events in 60s — enough resolution for the
  safe budget binary search to find the true ceiling (762 Mbps vs 425 Mbps with 24MB chunks)
- 3 workers × 16MB at 856 Mbps = 384 requests in 60s, zero stalls, 0.0 stability penalty
- 4 workers introduces occasional stalls (2.1s) — the single TCP connection can't feed
  4 parallel readers without gaps. 3 workers is the sweet spot.
- Larger chunks (24-32MB) reduce frontier event count, causing the binary search to
  underestimate safe budget by ~40% (coarser simulation resolution)

---

## 4. Simulator Pitfalls & Lessons for NEXIO

During development of the frontier simulator, we encountered and fixed several bugs that are directly applicable to NEXIO's Kotlin transport stack. Each represents a potential pitfall in the Android code.

### 4.1 Failed Requests Must Be Retried (Not Silently Dropped)

**Simulator bug:** When a request failed immediately (0 bytes, curl error), the retry logic only fired if `state.stalled == true`. But the stall timer requires 2 seconds of no progress — immediate failures complete in <100ms, so `stalled` was always `false`. Result: 71% of requests silently dropped, chunks permanently lost, frontier frozen.

**NEXIO risk:** Check `ParallelRangeDataSource` and `DualLaneScheduler` — if a range request throws `IOException` or `HttpException` before any bytes are received, does the chunk get re-queued? Or is it only the stall-detection path that triggers retry?

**Fix:** Retry on ANY incomplete transfer, not just stalls. Add an attempt counter with a cap (we use 10). Separate failure classification from retry eligibility.

```kotlin
// BAD: only retry on stall
if (state.isStalled && state.lane == Lane.URGENT) { requeue(chunk) }

// GOOD: retry any failure, classify separately
if (bytesReceived < expectedBytes && attempt < MAX_ATTEMPTS) { requeue(chunk) }
```

### 4.2 Connection Reuse Requires Handle Persistence

**Simulator bug:** Each request created a new `Easy2` handle, added it to the curl Multi, then removed and dropped it after transfer. Even with `set_max_host_connections()`, each new handle opened a fresh TCP+TLS connection because the handle that owned the cached connection was already destroyed.

**NEXIO risk:** Check if `OkHttpClient` connections are being reused across range requests in `ParallelRangeDataSource`. If each worker creates a new `Call` with a new URL object, OkHttp should pool the connection — but verify with network logs. If the server sends `Connection: close` (Real-Debrid does), OkHttp will close the connection and open a new one per request regardless.

**Fix:** For providers that enforce `Connection: close`, the transport layer must budget the connection rate — you cannot prevent new connections, but you can control how fast you open them.

### 4.3 Retry Backoff Is Critical — Instant Retry Creates Connection Storms

**Simulator bug:** Failed requests were retried immediately with zero delay. When the CDN started rate-limiting, 3 workers each failed and retried in a tight loop, generating 70+ new TLS connections per second (vs the CDN's ~10/sec tolerance). This amplified the rate limit from a brief hiccup into a 20+ second total blackout.

**NEXIO risk:** When `OkHttp` throws `ConnectException` or `SSLException` on an Android TV device, does the transport layer back off before retrying? Or does it immediately re-submit the chunk to the scheduler?

**Fix:** Exponential backoff on retry. Shorter backoff for frontier-blocking chunks (they're time-critical), longer for prefetch.

```kotlin
// Frontier-blocking chunk: 500ms, 1s, 2s, 4s
// Prefetch chunk: 2s, 4s, 8s, 16s
val backoffMs = if (isFrontierBlocker) {
    500L * (1 shl min(attempt, 4))
} else {
    2000L * (1 shl min(attempt, 4))
}
```

### 4.4 The Frontier Tracker Must Drive the Scheduler (Closed-Loop)

**Simulator bug:** Chunks were pre-generated in a flat FIFO queue with no feedback from the frontier tracker. When chunk 300 failed, nothing told the scheduler that chunk 300 was the one blocking all frontier progress. Later chunks completed successfully but couldn't advance contiguous playable bytes.

**NEXIO risk:** Does `DualLaneScheduler` know which chunk is blocking the frontier? When a chunk fails and gets re-queued, does it go to the back of the queue or does it get priority? The Kotlin `FrontierTracker` tracks contiguous bytes but does it feed back to the scheduler?

**Fix:** The frontier tracker must send its current contiguous byte position to the scheduler. Any failed chunk whose start byte is at or near the frontier position must be promoted to the urgent lane with priority.

```kotlin
// In scheduler, when requeuing a failed chunk:
val isFrontierBlocker = chunk.startByte <= frontierTracker.contiguousFrontierBytes + PAGE_SIZE
val lane = if (isFrontierBlocker) Lane.URGENT else chunk.originalLane
```

### 4.5 Safe Budget Must Include the Tail Drain

**Simulator bug:** The `ShadowPlayerSimulation` binary search only simulated playback through the frontier event timeline. If the frontier stopped advancing at 36s but the run continued to 120s, the simulator saw 36s of data and computed a bitrate that looked great — ignoring that a real player would have been draining its buffer for 84 additional seconds with zero new data.

**NEXIO risk:** When `ShadowPlayerSimulation.findMaxSustainableBitrate()` receives frontier events, does it account for the time between the last frontier event and the actual end of the benchmark run? If the run ended because the duration expired (not because all chunks completed), the simulation must drain the buffer through the remaining time.

**Fix:** Append a synthetic zero-delta frontier event at the run end timestamp before running the binary search.

```kotlin
// Before calling findMaxSustainableBitrate:
val events = frontierTracker.frontierEvents().toMutableList()
if (events.isNotEmpty()) {
    val lastEvent = events.last()
    val runEndMs = System.currentTimeMillis() - runStartMs
    if (runEndMs > lastEvent.tMs + 100) {
        events.add(FrontierEvent(
            tMs = runEndMs,
            contiguousFrontierBytes = lastEvent.contiguousFrontierBytes,
            deltaContiguousBytes = 0  // zero new data — forces buffer drain
        ))
    }
}
val result = simulation.findMaxSustainableBitrate(events)
```

### 4.6 TLS Backend Matters — Vendored OpenSSL vs System TLS

**Simulator bug:** Using vendored OpenSSL (built from source for CI cross-compilation) caused 80% TLS handshake failures with Real-Debrid's CDN. The system's SecureTransport (macOS native TLS) worked perfectly. The vendored OpenSSL likely had different cipher suite preferences or TLS version negotiation.

**NEXIO risk:** Android uses BoringSSL/Conscrypt as its TLS provider. This is generally well-maintained, but CDN-specific TLS issues can occur. If you see unexplained `SSLException` or `SSLHandshakeException` on specific providers, it could be a cipher suite mismatch, not a network issue.

**Diagnostic:** The frontier simulator's `--pcap` flag can capture TLS Client Hello and Server Hello to compare cipher suites and TLS versions between working and failing connections.

### 4.7 Protocol Detection Must Use Actual Negotiated Version

**Simulator bug:** The HTTP version field was incorrectly populated with a URL hash (copy-paste bug) and later with `http`/`https` scheme instead of the actual negotiated protocol. This masked whether the CDN was using HTTP/1.1 or HTTP/2.

**NEXIO risk:** When reporting benchmark telemetry to the shadow-collector, ensure the recorded protocol version comes from the actual HTTP response, not from assumptions. OkHttp's `Response.protocol()` returns the negotiated protocol.

### 4.8 Error Classification Must Be Specific

**Simulator bug:** All curl errors were initially classified as `ErrorClass::Other`, making it impossible to distinguish TLS failures from connection resets from timeouts. The trace showed 71% "errors" but gave no actionable information about what kind of errors.

**NEXIO risk:** `DebridBenchmarkMetricsCollector` should classify exceptions specifically:
- `SSLException` → TLS failure (possible cipher mismatch or cert issue)
- `ConnectException` → Connection refused or timeout
- `SocketException("Connection reset")` → Server killed the connection
- `SocketTimeoutException` → Read timeout (possible stall)
- `IOException` with partial bytes → Midstream failure (retry the suffix)

Each class implies a different recovery strategy.

---

## 5. Connection Budget Model

### 5.1 Real-Debrid Connection Budget

Based on pcap evidence, Real-Debrid's CDN enforces:

```
Rate limit: ~10 new TLS connections per second (sustained)
Hard cutoff: ~300 total connections in a rolling window (~30-60s)
Recovery: ~15-20 seconds of zero connections before accepting new ones
```

The transport layer should maintain a connection budget:

```kotlin
class ConnectionBudget(
    private val maxConnectionsPerSecond: Int = 8,  // safe margin below 10
    private val maxTotalConnections: Int = 250,     // safe margin below 300
    private val windowMs: Long = 60_000,
) {
    private val connectionTimestamps = ArrayDeque<Long>()

    @Synchronized
    fun canOpenConnection(): Boolean {
        val now = System.currentTimeMillis()
        // Prune old timestamps
        while (connectionTimestamps.isNotEmpty() && 
               now - connectionTimestamps.first() > windowMs) {
            connectionTimestamps.removeFirst()
        }
        // Check total in window
        if (connectionTimestamps.size >= maxTotalConnections) return false
        // Check rate (last second)
        val recentCount = connectionTimestamps.count { now - it < 1000 }
        return recentCount < maxConnectionsPerSecond
    }

    @Synchronized
    fun recordConnection() {
        connectionTimestamps.addLast(System.currentTimeMillis())
    }
}
```

### 5.2 Premiumize: No Budget Needed

Premiumize supports full keep-alive. The connection budget is not needed — a single connection handles all traffic. The transport layer should detect this via the first response's `Connection` header and disable connection budgeting.

---

## 6. Recommended Changes to NEXIO Code

### 6.1 TransportPolicyController

Add provider-specific profiles:

```kotlin
fun policyForProvider(provider: DebridBenchmarkProvider, envelope: CapabilityEnvelope): TransportPolicy {
    return when (provider) {
        REAL_DEBRID -> TransportPolicy(
            urgentWorkers = min(envelope.maxSafeUrgentWorkers, 2),
            prefetchWorkers = 1,
            urgentChunkBytes = max(envelope.maxSafeUrgentChunkBytes, 16 * 1024 * 1024),
            prefetchChunkBytes = max(envelope.maxSafePrefetchChunkBytes, 32 * 1024 * 1024),
        )
        PREMIUMIZE -> TransportPolicy(
            urgentWorkers = min(envelope.maxSafeUrgentWorkers, 3),
            prefetchWorkers = 1,
            urgentChunkBytes = 8 * 1024 * 1024,
            prefetchChunkBytes = 16 * 1024 * 1024,
        )
        else -> TransportPolicy.fromEnvelope(envelope) // default
    }
}
```

### 6.2 DualLaneScheduler

Add frontier feedback and connection budgeting:

```kotlin
class DualLaneScheduler(
    private val connectionBudget: ConnectionBudget? = null,  // null for keep-alive providers
) {
    fun submitUrgent(chunk: ChunkRequest) {
        // Urgent lane: always submit, but respect connection budget
        connectionBudget?.let {
            while (!it.canOpenConnection()) {
                Thread.sleep(100)
            }
            it.recordConnection()
        }
        urgentExecutor.submit(chunk)
    }
}
```

### 6.3 CapabilityEnvelope

Add provider transport characteristics:

```kotlin
data class CapabilityEnvelope(
    // ... existing fields ...
    val supportsKeepAlive: Boolean = true,
    val maxConnectionsPerSecond: Int? = null,  // null = unlimited
    val requestsPerConnection: Int? = null,    // null = unlimited (keep-alive)
    val measuredProviderBehavior: String? = null, // "connection_close" | "keep_alive"
)
```

### 6.4 DebridConfigBenchmarkService

During benchmarking, detect the provider's connection behavior:

```kotlin
// After first successful range request, check Connection header
val connectionHeader = response.header("Connection")
val supportsKeepAlive = connectionHeader?.lowercase() != "close"
```

---

## 7. Benchmark Data Summary

### 7.1 Real-Debrid (CDN: *.download.real-debrid.com)

| Chunk Size | Workers | Duration | Requests | Connections | Retries | Stalls | Max Stall | Throughput | Safe Budget | Stability |
|-----------|---------|----------|----------|-------------|---------|--------|-----------|------------|-------------|-----------|
| 4 MB | 3 | 60s | 890 | 311 | 442 | 2 | 9.5s | 249 Mbps | 183 Mbps | 0.7 |
| 4 MB | 3 | 120s | 3887 | 3767 | 3127 | 2 | 26.4s | 195 Mbps | 153 Mbps | 0.7 |
| 8 MB | 2 | 120s | 562 | 282 | 0 | 0 | 0.4s | 313 Mbps | 273 Mbps | 0.0 |
| 16 MB | 3 | 60s | 257 | 92 | 0 | 1 | 5.8s | 571 Mbps | 425 Mbps | 0.6 |
| 24 MB | 2 | 60s | 170 | 85 | 0 | 0 | 1.0s | 564 Mbps | 425 Mbps | 0.0 |
| 32 MB | 2 | 60s | 136 | 68 | 0 | 0 | 0.4s | 604 Mbps | 425 Mbps | 0.0 |

### 7.2 Premiumize (CDN: energycdn.com)

| Chunk Size | Workers | Duration | Requests | Connections | Retries | Stalls | Max Stall | Throughput | Safe Budget | Stability |
|-----------|---------|----------|----------|-------------|---------|--------|-----------|------------|-------------|-----------|
| **16 MB** | **3** | **60s** | **384** | **1** | **0** | **0** | **0.3s** | **856 Mbps** | **762 Mbps** | **0.0** |
| 16 MB | 4 | 60s | 388 | 1 | 0 | 1 | 2.1s | 856 Mbps | 756 Mbps | 0.4 |
| 16 MB | 2 | 60s | 376 | 1 | 0 | 0 | 0.2s | 838 Mbps | 752 Mbps | 0.0 |
| 24 MB | 1 | 60s | 231 | 1 | 0 | 0 | 0.2s | 771 Mbps | 425 Mbps | 0.0 |
| 24 MB | 2 | 60s | 235 | 1 | 0 | 0 | 0.8s | 784 Mbps | 425 Mbps | 0.0 |
| 24 MB | 3 | 60s | 227 | 1 | 0 | 1 | 2.3s | 713 Mbps | 425 Mbps | 0.4 |
| 32 MB | 2 | 60s | 188 | 1 | 0 | 0 | 1.8s | 771 Mbps | 425 Mbps | 0.0 |
| 32 MB | 3 | 60s | 181 | 1 | 0 | 0 | 1.2s | 719 Mbps | 425 Mbps | 0.0 |

**Key insight:** With Premiumize's keep-alive connection, chunk size affects measurement accuracy
more than transport performance. Smaller chunks produce more frontier events, giving the binary
search simulation higher resolution. At 24-32MB the safe budget appears to cap at 425 Mbps, but
at 16MB with the same throughput (~856 Mbps) the safe budget correctly measures 762 Mbps.

### 7.3 Wire-Level Evidence

| Evidence | File | Key Finding |
|----------|------|-------------|
| RST source | `traces/run_1775489793889/capture.pcap` | 1285 RST packets, ALL from CDN IP (162.19.10.28), zero from client |
| Connection close | Same pcap, streams 0-19 | Every TCP stream: exactly ~4.2 MB, closed by FIN from CDN |
| TLS rate | Same pcap, Client Hello timing | 6-10/sec healthy, 70+/sec during retry storm, cutoff at 38s |
| Connection reuse (PM) | `traces/run_1775490436544/trace.jsonl` | 235 requests, 1 connection — full keep-alive |

---

## 8. Validation Checklist for NEXIO

Before deploying transport changes, validate with the frontier simulator:

- [ ] Run 120s benchmark with each provider using the new chunk sizes
- [ ] Confirm Real-Debrid: zero retries, zero stalls with 16MB+ chunks
- [ ] Confirm Premiumize: single connection, zero stalls
- [ ] Compare Kotlin `ShadowPlayerSimulation` output with Rust `SafeBudgetEstimator` for the same frontier events
- [ ] Verify `TransportPolicyController` selects the correct profile per provider
- [ ] Verify `DualLaneScheduler` requeues failed chunks to urgent lane when frontier-blocking
- [ ] Verify connection budget prevents retry storms on Real-Debrid
- [ ] Capture pcap from Android TV (via `tcpdump` over ADB) and compare connection patterns with simulator

---

## 9. Future Work

1. **Per-provider auto-tuning**: Run the simulator with increasing chunk sizes to automatically discover each provider's optimal config
2. **HTTP/2 testing**: Some CDN nodes may support HTTP/2 multiplexing which changes the connection model entirely
3. **Torbox + Easy-Debrid profiling**: Extend provider adapters and run the same benchmark suite
4. **Seek TTFB profiling**: Measure time-to-first-byte after a seek (new range request from cold start)
5. **Kotlin trace replay validation**: Feed simulator JSONL traces into the Kotlin `FrontierTracker` and diff the frontier curves
6. **Automated regression**: CI job that runs benchmarks and flags throughput regressions
