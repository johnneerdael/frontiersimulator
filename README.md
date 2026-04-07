# Frontier Simulator

A Rust CLI transport lab for benchmarking debrid provider download performance on macOS. Provides ground-truth network telemetry (jitter, latency, stalls, bandwidth fluctuation, connection resets) that in-app Android TV benchmarks cannot capture.

Designed to validate the Kotlin frontier simulator in [NEXIO](https://github.com/johnneerdael/nexio) by generating canonical JSONL traces that can be replayed through both Rust and Kotlin simulators.

## Features

- **Transport-level benchmarking** via libcurl multi with full control over range requests and byte handling
- **128KB page-based frontier tracking** matching the Kotlin `FrontierTracker` implementation
- **Parallel range requests** with urgent/prefetch lane scheduling and connection reuse tracking
- **Stall detection** using page-arrival-based no-progress timers (not libcurl low-speed)
- **Suffix resume** for stalled urgent requests
- **Shadow player simulation** with binary search for max sustainable bitrate
- **Capability envelope generation** compatible with Kotlin's `CapabilityEnvelope.fromJson()`
- **JSONL trace output** for cross-platform validation
- **SQLite storage** for cross-run queries and comparison
- **Real-Debrid and Premiumize** provider support (extensible to Torbox and Easy-Debrid)

## Requirements

- macOS (primary target)
- Rust stable toolchain (1.75+)
- libcurl is vendored via `curl-sys` (no system dependency needed)

## Installation

### From source

```bash
git clone https://github.com/johnneerdael/frontiersimulator.git
cd frontiersimulator
cargo build --release
```

The binary will be at `target/release/frontier-sim`.

### From GitHub Releases

Download the latest pre-built macOS binary from the [Releases](https://github.com/johnneerdael/frontiersimulator/releases) page.

## Configuration

Create a `frontier.toml` file (or copy from `frontier.toml.example`):

```toml
[providers.realdebrid]
api_key = "YOUR_REALDEBRID_API_KEY"

[providers.premiumize]
api_key = "YOUR_PREMIUMIZE_API_KEY"

[defaults]
chunk_size_mb = 4        # Urgent lane chunk size for range requests
prefetch_chunk_size_mb = 8 # Prefetch lane chunk size for range requests
page_size_kb = 128       # Page size for frontier tracking (matches Kotlin)
urgent_workers = 2       # Concurrent urgent lane workers
prefetch_workers = 1     # Fixed by the simulator/runtime
protocol = "auto"        # auto | h1 | h2
sink = "discard"         # discard | sparse-file | rolling-ring

[output]
trace_dir = "./traces"   # Directory for JSONL trace files
db_path = "./frontier.db" # SQLite database path
```

### Environment variables

API keys can also be set via environment variables (takes precedence over config file):

```bash
export REALDEBRID_API_KEY="your_key_here"
export PREMIUMIZE_API_KEY="your_key_here"
```

### CLI flags

CLI flags override both config file and environment variables. Use `--config` to specify a different config file path.
`--chunk-size-mb` controls the urgent lane, and `--prefetch-chunk-size-mb` controls the prefetch lane.

## Usage

### Show configuration

```bash
frontier-sim config
```

Displays the loaded configuration including provider status, default parameters, and output paths.

### List provider assets

```bash
# List assets from all configured providers
frontier-sim providers list

# List assets from a specific provider
frontier-sim providers list --provider realdebrid
frontier-sim providers list --provider premiumize
```

Shows available video assets sorted by size (largest first), with stable asset keys for use in benchmarks.

### Probe a URL

```bash
frontier-sim probe https://example.com/video.mkv
```

Probes a URL to determine:
- Range request support (`Accept-Ranges: bytes`)
- Content length
- Protocol version (HTTP/1.1, HTTP/2)
- Connection endpoints (local/remote IP:port)
- Timing facts (DNS, connect, TLS, TTFB)

### Run a benchmark

The benchmark command accepts either a **stable asset key** (recommended) or a raw URL.

**Using an asset key (recommended workflow):**

```bash
# Step 1: List available assets from your provider
frontier-sim providers list --provider realdebrid

# Output:
#   [0] Movie.2160p.BluRay.REMUX.mkv (50000.0 MB) [realdebrid:ABC123:1:52428800000]
#   [1] Show.S01E01.1080p.mkv (5000.0 MB) [realdebrid:DEF456:3:5242880000]

# Step 2: Copy the asset key from the listing and run the benchmark
frontier-sim benchmark realdebrid:ABC123:1:52428800000
```

The tool will resolve the asset key to a direct download URL via the provider API (`/unrestrict/link` for Real-Debrid, `/item/details` for Premiumize) before probing and benchmarking.

**Options:**

```bash
# Default: 120 second sustained benchmark (matching Android TV)
frontier-sim benchmark realdebrid:ABC123:1:52428800000

# Shorter benchmark (30 seconds)
frontier-sim benchmark realdebrid:ABC123:1:52428800000 --duration 30

# Limit by bytes instead of time
frontier-sim benchmark realdebrid:ABC123:1:52428800000 --max-bytes 67108864

# Specify output directory
frontier-sim benchmark realdebrid:ABC123:1:52428800000 --output ./my-traces

# Benchmark a direct URL
frontier-sim benchmark https://example.com/video.mkv
```

Runs a full parallel benchmark:
1. Resolves asset key to direct URL (if asset key provided)
2. Probes the URL for range support and content length
3. Splits the download into chunks across urgent and prefetch lanes
4. Downloads chunks in parallel via libcurl multi
5. Tracks page-level frontier advancement (128KB pages)
6. Detects stalls and performs suffix resume for urgent requests
7. Runs shadow player simulation to find max sustainable bitrate
8. Generates a capability envelope
9. Writes JSONL trace + JSON summary + SQLite record

**Output files:**

```
traces/
  run_<timestamp>/
    trace.jsonl      # Raw event stream (all telemetry events)
    summary.json     # Run summary with capability envelope
frontier.db          # SQLite database for cross-run queries
```

### Download a single range

```bash
# Download first 4MB
frontier-sim download-single https://example.com/video.mkv

# Download a specific range
frontier-sim download-single https://example.com/video.mkv \
  --range-start 1048576 --range-end 5242879

# Specify output trace file
frontier-sim download-single https://example.com/video.mkv -o my-trace.jsonl
```

Downloads a single byte range and produces a JSONL trace. Useful for debugging individual transfer behavior.

### Query past runs

```bash
# List all benchmark runs
frontier-sim runs list

# Show details of a specific run
frontier-sim runs show run_1712345678000
```

Queries the SQLite database for past benchmark runs, showing capability envelopes, request statistics, and run summaries.

## JSONL Trace Format

Each line in the trace file is a JSON object with an `event_type` field:

| Event Type | Description |
|-----------|-------------|
| `run_started` | Benchmark run begins |
| `probe_completed` | URL probe results |
| `request_started` | Range request initiated |
| `request_prereq` | Connection facts (IP, port, protocol, reuse) |
| `headers_received` | HTTP response headers |
| `page_completed` | 128KB page fully received |
| `request_stalled` | No progress detected (>2000ms) |
| `request_resumed` | Suffix resume after stall |
| `request_finished` | Transfer complete with full timing |
| `frontier_advanced` | Contiguous frontier moved forward |
| `connection_lifecycle` | Connection summary at run end |
| `run_completed` | Benchmark run finished |

### Example trace event

```json
{"event_type":"page_completed","request_id":1,"chunk_id":0,"page_index":5,"page_start_byte":655360,"page_end_byte":786432,"page_bytes":131072,"cumulative_bytes":786432,"timestamp_ns":150000000}
```

## Capability Envelope

The benchmark produces a capability envelope compatible with the Kotlin `CapabilityEnvelope`:

```json
{
  "maxSafeUrgentWorkers": 2,
  "maxSafePrefetchWorkers": 1,
  "maxSafeUrgentChunkBytes": 4194304,
  "maxSafePrefetchChunkBytes": 8388608,
  "sustainedThroughputMbps": 75.5,
  "stabilityPenalty": 0.1,
  "supportsRangeRequests": true,
  "measuredAtMs": 1712345678000
}
```

The benchmark also records both chunk sizes explicitly in `trace.jsonl`, `summary.json` (`benchmark_config`), and SQLite `config_json`.

Optional fields (`p10ThroughputMbps`, `seekTtfbP50Ms`) are omitted when not available, matching Kotlin's serialization behavior.

## Kotlin Validation Workflow

The primary purpose of this tool is to validate the Kotlin frontier simulator:

1. Run a macOS benchmark to generate a JSONL trace with `frontier_advanced` events
2. Feed the same frontier events into the Kotlin `ShadowPlayerSimulation`
3. Compare `safeBudgetMbps` and `maxSustainableBitrateMbps` between Rust and Kotlin
4. Investigate divergence in frontier curves

The Rust `SafeBudgetEstimator` uses identical constants and algorithm as the Kotlin `ShadowPlayerSimulation`:
- Startup buffer: 3000ms
- Max buffer: 50000ms
- Safety factor: 0.85
- Search tolerance: 0.1 Mbps
- Bitrate range: 1.0 - 500.0 Mbps

## Architecture

```
CLI (clap)
  |
  +-- Provider Control Plane (ureq)
  |     +-- RealDebridProvider
  |     +-- PremiumizeProvider
  |
  +-- Probe Service (libcurl)
  |
  +-- Transfer Reactor (libcurl multi + curl_multi_poll)
  |     +-- Connection Registry (CURLINFO_CONN_ID)
  |     +-- Stall Detection (page-arrival timer)
  |     +-- Suffix Resume
  |
  +-- Frontier Tracker (128KB pages, BitSet)
  |     +-- Safe Budget Estimator (binary search)
  |     +-- Capability Envelope Builder
  |
  +-- Telemetry
        +-- JSONL Writer (crossbeam channel + background thread)
        +-- SQLite Storage (rusqlite, bundled)
```

## Development

```bash
# Build (debug)
cargo build

# Build (release)
cargo build --release

# Run tests
cargo test

# Run a specific test
cargo test -- frontier_runner_processes_pages

# Check without building
cargo check
```

## License

Private - NEXIO project.
