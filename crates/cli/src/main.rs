mod config;

use clap::{Parser, Subcommand};
use frontier_provider::{DebridProvider, realdebrid::RealDebridProvider, premiumize::PremiumizeProvider};
use frontier_core::{run_frontier_tracker, FrontierRunConfig};
use frontier_telemetry::sqlite as db;
use frontier_telemetry::jsonl;
use frontier_transport::download::{self, DownloadParams};
use frontier_transport::probe;
use frontier_transport::reactor::{self, ChunkRequest, ReactorConfig};
use frontier_telemetry::events::Lane;
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Parser)]
#[command(name = "frontier-sim")]
#[command(about = "macOS frontier simulator — transport-level benchmarking for debrid providers")]
#[command(version)]
struct Cli {
    /// Path to config file (default: frontier.toml)
    #[arg(short, long, default_value = "frontier.toml")]
    config: String,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Show loaded configuration
    Config,

    /// List available assets from configured providers
    Providers {
        #[command(subcommand)]
        action: ProviderAction,
    },

    /// Probe a URL or asset key for range support and timing facts
    Probe {
        /// URL or asset key (e.g., realdebrid:LU3DESWCEVAM4:1:169114151864)
        target: String,
    },

    /// Run a full parallel benchmark against a URL or asset key
    Benchmark {
        /// URL or asset key (e.g., realdebrid:LU3DESWCEVAM4:1:169114151864)
        target: String,

        /// Benchmark duration in seconds (default: 120s, matching Android TV)
        #[arg(long, default_value = "120")]
        duration: u64,

        /// Max bytes to download (0 = unlimited, constrained by duration)
        #[arg(long, default_value = "0")]
        max_bytes: u64,

        /// Chunk size in MB (default: from config, typically 4)
        #[arg(long)]
        chunk_size_mb: Option<u32>,

        /// Number of urgent workers (default: from config)
        #[arg(long)]
        workers: Option<u32>,

        /// Output directory for trace files
        #[arg(short, long)]
        output: Option<String>,
    },

    /// Query past benchmark runs from SQLite
    Runs {
        #[command(subcommand)]
        action: RunsAction,
    },

    /// Download a single byte range and produce a JSONL trace
    DownloadSingle {
        /// URL to download from
        url: String,

        /// Byte range start
        #[arg(long, default_value = "0")]
        range_start: u64,

        /// Byte range end (defaults to range_start + 4MB)
        #[arg(long)]
        range_end: Option<u64>,

        /// Output JSONL trace file
        #[arg(short, long, default_value = "trace.jsonl")]
        output: String,
    },
}

#[derive(Subcommand)]
enum ProviderAction {
    /// List available assets
    List {
        /// Provider name (realdebrid, premiumize)
        #[arg(short, long)]
        provider: Option<String>,
    },
}

#[derive(Subcommand)]
enum RunsAction {
    /// List all past benchmark runs
    List,
    /// Show details of a specific run
    Show {
        /// Run ID
        run_id: String,
    },
}

fn main() {
    let cli = Cli::parse();

    let cfg = match config::Config::load(&cli.config) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to load config: {e}");
            std::process::exit(1);
        }
    };

    match cli.command {
        Commands::Config => {
            println!("Configuration loaded from: {}", cli.config);
            println!();
            println!("Providers:");
            if let Some(ref rd) = cfg.providers.realdebrid {
                println!("  realdebrid: API key {}configured", if rd.api_key.is_empty() { "NOT " } else { "" });
            } else {
                println!("  realdebrid: not configured");
            }
            if let Some(ref pm) = cfg.providers.premiumize {
                println!("  premiumize: API key {}configured", if pm.api_key.is_empty() { "NOT " } else { "" });
            } else {
                println!("  premiumize: not configured");
            }
            println!();
            println!("Defaults:");
            println!("  chunk_size_mb:    {}", cfg.defaults.chunk_size_mb);
            println!("  page_size_kb:     {}", cfg.defaults.page_size_kb);
            println!("  urgent_workers:   {}", cfg.defaults.urgent_workers);
            println!("  prefetch_workers: {}", cfg.defaults.prefetch_workers);
            println!("  protocol:         {:?}", cfg.defaults.protocol);
            println!("  sink:             {:?}", cfg.defaults.sink);
            println!();
            println!("Output:");
            println!("  trace_dir: {}", cfg.output.trace_dir);
            println!("  db_path:   {}", cfg.output.db_path);
        }

        Commands::Providers { action } => match action {
            ProviderAction::List { provider } => {
                let providers = build_providers(&cfg, provider.as_deref());
                if providers.is_empty() {
                    eprintln!("No providers configured. Set API keys in frontier.toml or via environment variables.");
                    std::process::exit(1);
                }
                for p in &providers {
                    println!("Provider: {}", p.name());
                    match p.list_candidates() {
                        Ok(candidates) => {
                            if candidates.is_empty() {
                                println!("  No playable video assets found.");
                            }
                            for (i, asset) in candidates.iter().enumerate() {
                                let size_mb = asset.key.size_bytes as f64 / 1_048_576.0;
                                println!(
                                    "  [{i}] {} ({:.1} MB) [{}]",
                                    asset.filename, size_mb, asset.key.canonical()
                                );
                            }
                        }
                        Err(e) => {
                            eprintln!("  Error listing assets: {e}");
                        }
                    }
                    println!();
                }
            }
        },

        Commands::Probe { target } => {
            let url = resolve_target(&target, &cfg);
            println!("Probing: {url}");
            match probe::probe_url(&url, Duration::from_secs(30)) {
                Ok(result) => {
                    println!();
                    println!("Range support:  {}", result.range_support);
                    println!(
                        "Content length: {}",
                        result
                            .content_length
                            .map(|l| format!("{l} bytes ({:.1} MB)", l as f64 / 1_048_576.0))
                            .unwrap_or_else(|| "unknown".to_string())
                    );
                    println!("Protocol:       {}", result.protocol);
                    println!("Remote:         {}", result.remote_endpoint);
                    println!("Local:          {}", result.local_endpoint);
                    println!();
                    println!("Timing:");
                    println!("  DNS:     {:.1} ms", result.dns_ms);
                    println!("  Connect: {:.1} ms", result.connect_ms);
                    println!("  TLS:     {:.1} ms", result.tls_ms);
                    println!("  TTFB:    {:.1} ms", result.ttfb_ms);
                    println!();
                    println!("URL hash: {}", result.effective_url_hash);
                }
                Err(e) => {
                    eprintln!("Probe failed: {e}");
                    std::process::exit(1);
                }
            }
        }

        Commands::Benchmark { target, duration, max_bytes, chunk_size_mb, workers, output } => {
            let page_size = (cfg.defaults.page_size_kb as u64) * 1024;
            let effective_chunk_mb = chunk_size_mb.unwrap_or(cfg.defaults.chunk_size_mb);
            let chunk_size = (effective_chunk_mb as u64) * 1024 * 1024;
            let effective_urgent_workers = workers.unwrap_or(cfg.defaults.urgent_workers);
            let trace_dir = output.unwrap_or_else(|| cfg.output.trace_dir.clone());
            let run_id = format!(
                "run_{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis()
            );
            let trace_path = PathBuf::from(&trace_dir).join(&run_id).join("trace.jsonl");
            let summary_path = PathBuf::from(&trace_dir).join(&run_id).join("summary.json");

            let url = resolve_target(&target, &cfg);
            let asset_key = target.clone();

            // Probe first
            println!("Probing: {url}");
            let probe_result = match probe::probe_url(&url, Duration::from_secs(30)) {
                Ok(r) => {
                    println!("  Range support: {}, Content length: {:?}, Protocol: {}",
                        r.range_support,
                        r.content_length.map(|l| format!("{:.1} MB", l as f64 / 1_048_576.0)),
                        r.protocol);
                    r
                }
                Err(e) => {
                    eprintln!("Probe failed: {e}");
                    std::process::exit(1);
                }
            };

            // Determine how much of the file to use for benchmarking.
            // Generate chunks for the full file (or max_bytes if set), but the reactor
            // will stop starting new chunks once the duration limit expires.
            let file_size = probe_result.content_length.unwrap_or(u64::MAX);
            let download_size = if max_bytes > 0 {
                max_bytes.min(file_size)
            } else {
                file_size // entire file — reactor will stop when duration expires
            };

            // Build chunk list
            let mut chunks = Vec::new();
            let mut offset = 0u64;
            let mut chunk_id = 0u64;
            while offset < download_size {
                let end = (offset + chunk_size - 1).min(download_size - 1);
                let lane = if chunks.is_empty() || offset < chunk_size * 2 {
                    Lane::Urgent
                } else {
                    Lane::Prefetch
                };
                chunks.push(ChunkRequest {
                    chunk_id,
                    range_start: offset,
                    range_end: end,
                    lane,
                });
                offset = end + 1;
                chunk_id += 1;
                // Cap chunk list to avoid huge memory for very large files
                if chunks.len() >= 10_000 {
                    break;
                }
            }

            println!(
                "Benchmarking: {} chunks queued ({} MB chunks), {:.1} MB available, {}s duration, {} urgent + {} prefetch workers",
                chunks.len(),
                effective_chunk_mb,
                download_size as f64 / 1_048_576.0,
                duration,
                effective_urgent_workers,
                cfg.defaults.prefetch_workers,
            );

            // Set up channels: reactor -> frontier tracker -> JSONL writer
            let (reactor_tx, reactor_rx) = crossbeam_channel::unbounded();
            let (frontier_tx, frontier_rx) = crossbeam_channel::unbounded();

            let writer_handle = match jsonl::spawn_jsonl_writer(frontier_rx, &trace_path) {
                Ok(h) => h,
                Err(e) => {
                    eprintln!("Failed to create JSONL writer: {e}");
                    std::process::exit(1);
                }
            };

            let run_start = Instant::now();

            // Emit run_started
            let _ = reactor_tx.send(frontier_telemetry::TraceEvent::RunStarted {
                run_id: run_id.clone(),
                asset_key: asset_key.clone(),
                config: serde_json::json!({
                    "chunk_size_mb": effective_chunk_mb,
                    "page_size_kb": cfg.defaults.page_size_kb,
                    "urgent_workers": effective_urgent_workers,
                    "prefetch_workers": cfg.defaults.prefetch_workers,
                    "sink": format!("{:?}", cfg.defaults.sink),
                }),
                timestamp_ns: 0,
            });

            // Spawn frontier tracker thread with feedback channel to reactor
            let frontier_config = FrontierRunConfig {
                page_size,
                chunk_size_bytes: chunk_size,
                urgent_workers: effective_urgent_workers,
                prefetch_workers: cfg.defaults.prefetch_workers,
                range_support: probe_result.range_support,
            };

            // Feedback channel: frontier tracker → reactor (frontier byte position)
            let (feedback_tx, feedback_rx) = crossbeam_channel::unbounded::<u64>();

            let frontier_handle = std::thread::spawn(move || {
                run_frontier_tracker(reactor_rx, frontier_tx, Some(feedback_tx), run_start, frontier_config)
            });

            // Run reactor with frontier feedback
            let reactor_config = ReactorConfig {
                urgent_workers: effective_urgent_workers,
                prefetch_workers: cfg.defaults.prefetch_workers,
                page_size,
                sink: cfg.defaults.sink,
                max_duration: Duration::from_secs(duration),
                ..ReactorConfig::default()
            };

            let reactor_result =
                reactor::run_parallel(&url, &chunks, &reactor_config, &reactor_tx, Some(feedback_rx), run_start);

            // Close reactor channel to signal frontier tracker
            drop(reactor_tx);

            // Wait for frontier tracker
            let frontier_result = frontier_handle.join().expect("frontier thread panicked");

            // Emit run_completed would go here but channel is closed — it's in the trace via frontier

            // Wait for JSONL writer
            match writer_handle.join() {
                Ok(Ok(count)) => println!("Trace: {count} events -> {}", trace_path.display()),
                Ok(Err(e)) => eprintln!("JSONL writer error: {e}"),
                Err(_) => eprintln!("JSONL writer panicked"),
            }

            // Write summary
            let summary = serde_json::json!({
                "run_id": run_id,
                "url": url,
                "asset_key": asset_key,
                "download_size": download_size,
                "chunks": chunks.len(),
                "reactor": {
                    "total_bytes": reactor_result.as_ref().map(|r| r.total_bytes).unwrap_or(0),
                    "total_requests": reactor_result.as_ref().map(|r| r.total_requests).unwrap_or(0),
                    "total_connections": reactor_result.as_ref().map(|r| r.total_connections).unwrap_or(0),
                    "duration_ms": reactor_result.as_ref().map(|r| r.duration_ms).unwrap_or(0.0),
                },
                "frontier": {
                    "final_frontier_bytes": frontier_result.final_frontier_bytes,
                    "frontier_events": frontier_result.frontier_events_count,
                    "advance_rate_mbps": frontier_result.frontier_advance_rate_mbps,
                    "stall_count": frontier_result.frontier_stall_count,
                    "max_stall_ms": frontier_result.max_frontier_stall_ms,
                    "tail_drain_ms": frontier_result.tail_drain_ms,
                    "run_duration_ms": frontier_result.run_duration_ms,
                    "max_sustainable_bitrate_mbps": frontier_result.max_sustainable_bitrate_mbps,
                    "safe_budget_mbps": frontier_result.safe_budget_mbps,
                },
                "capability_envelope": serde_json::to_value(&frontier_result.envelope).unwrap_or_default(),
            });

            if let Some(parent) = summary_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Ok(file) = std::fs::File::create(&summary_path) {
                let _ = serde_json::to_writer_pretty(file, &summary);
                println!("Summary: {}", summary_path.display());
            }

            // Print results
            println!();
            println!("=== Benchmark Results ===");
            if let Ok(ref r) = reactor_result {
                println!("Transfer: {:.1} MB in {:.1}s ({:.1} Mbps)",
                    r.total_bytes as f64 / 1_048_576.0,
                    r.duration_ms / 1000.0,
                    r.total_bytes as f64 * 8.0 / r.duration_ms / 1000.0,
                );
                println!("Requests: {} across {} connections ({} retried, {} failed)",
                    r.total_requests, r.total_connections, r.retried_chunks, r.failed_chunks);
            }
            println!("Frontier: {:.1} MB contiguous, {} events, {} stalls (max {:.1}s)",
                frontier_result.final_frontier_bytes as f64 / 1_048_576.0,
                frontier_result.frontier_events_count,
                frontier_result.frontier_stall_count,
                frontier_result.max_frontier_stall_ms as f64 / 1000.0,
            );
            if frontier_result.tail_drain_ms > 2_000 {
                println!("Tail drain: {:.1}s (frontier frozen from {:.1}s to {:.1}s)",
                    frontier_result.tail_drain_ms as f64 / 1000.0,
                    (frontier_result.run_duration_ms - frontier_result.tail_drain_ms) as f64 / 1000.0,
                    frontier_result.run_duration_ms as f64 / 1000.0,
                );
            }
            if let Some(rate) = frontier_result.frontier_advance_rate_mbps {
                println!("Frontier rate: {:.1} Mbps", rate);
            }
            println!("Max sustainable: {:.1} Mbps", frontier_result.max_sustainable_bitrate_mbps);
            println!("Safe budget:     {:.1} Mbps", frontier_result.safe_budget_mbps);
            println!();
            println!("Capability Envelope:");
            println!("  {}", serde_json::to_string_pretty(&frontier_result.envelope).unwrap_or_default());

            // Store in SQLite
            let db_path = std::path::Path::new(&cfg.output.db_path);
            if let Ok(db_conn) = db::init_db(db_path) {
                let started_at_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as i64;
                let config_json = serde_json::json!({
                    "chunk_size_mb": effective_chunk_mb,
                    "page_size_kb": cfg.defaults.page_size_kb,
                    "urgent_workers": effective_urgent_workers,
                    "prefetch_workers": cfg.defaults.prefetch_workers,
                }).to_string();
                let _ = db::insert_run(&db_conn, &run_id, &asset_key, "", started_at_ms, &config_json);
                let envelope_json = serde_json::to_string(&frontier_result.envelope).unwrap_or_default();
                let sum_json = serde_json::to_string(&summary).unwrap_or_default();
                let _ = db::finish_run(&db_conn, &run_id, started_at_ms, &envelope_json, &sum_json);

                // Store connection records
                if let Ok(ref r) = reactor_result {
                    for conn_rec in &r.connections {
                        let _ = db::insert_connection(
                            &db_conn, &run_id, conn_rec.connection_id,
                            &conn_rec.protocol, &conn_rec.local_endpoint, &conn_rec.remote_endpoint,
                            conn_rec.first_use_ns as f64 / 1_000_000.0,
                            conn_rec.last_use_ns as f64 / 1_000_000.0,
                            conn_rec.requests_count, conn_rec.bytes_total,
                        );
                    }
                }

                println!("SQLite: {}", db_path.display());
            }
        }

        Commands::Runs { action } => {
            let db_path = std::path::Path::new(&cfg.output.db_path);
            let db_conn = match db::init_db(db_path) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("Failed to open database: {e}");
                    std::process::exit(1);
                }
            };
            match action {
                RunsAction::List => {
                    match db::list_runs(&db_conn) {
                        Ok(runs) => {
                            if runs.is_empty() {
                                println!("No benchmark runs found.");
                            } else {
                                println!("{:<30} {:<12} {:<40} {}", "RUN ID", "PROVIDER", "ASSET", "ENVELOPE");
                                println!("{}", "-".repeat(100));
                                for run in &runs {
                                    let envelope_summary = run.envelope_json.as_deref()
                                        .and_then(|j| serde_json::from_str::<serde_json::Value>(j).ok())
                                        .map(|v| format!(
                                            "{:.0} Mbps, {} workers",
                                            v["sustainedThroughputMbps"].as_f64().unwrap_or(0.0),
                                            v["maxSafeUrgentWorkers"].as_u64().unwrap_or(0),
                                        ))
                                        .unwrap_or_else(|| "pending".to_string());
                                    let asset_short = if run.asset_key.len() > 38 {
                                        format!("{}...", &run.asset_key[..35])
                                    } else {
                                        run.asset_key.clone()
                                    };
                                    println!("{:<30} {:<12} {:<40} {}", run.run_id, run.provider, asset_short, envelope_summary);
                                }
                            }
                        }
                        Err(e) => eprintln!("Error listing runs: {e}"),
                    }
                }
                RunsAction::Show { run_id } => {
                    match db::get_run(&db_conn, &run_id) {
                        Ok(Some(run)) => {
                            println!("Run: {}", run.run_id);
                            println!("Asset: {}", run.asset_key);
                            println!("Provider: {}", run.provider);
                            println!();
                            if let Some(ref summary) = run.summary_json {
                                if let Ok(v) = serde_json::from_str::<serde_json::Value>(summary) {
                                    println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
                                }
                            }
                            if let Some(ref envelope) = run.envelope_json {
                                println!();
                                println!("Capability Envelope:");
                                if let Ok(v) = serde_json::from_str::<serde_json::Value>(envelope) {
                                    println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
                                }
                            }
                            // Request stats
                            if let Ok((count, avg_ttfb, avg_total, sum_bytes)) = db::get_request_stats(&db_conn, &run_id) {
                                println!();
                                println!("Request Stats:");
                                println!("  Total requests:  {count}");
                                println!("  Avg TTFB:        {avg_ttfb:.1} ms");
                                println!("  Avg total time:  {avg_total:.1} ms");
                                println!("  Total bytes:     {sum_bytes:.0}");
                            }
                        }
                        Ok(None) => {
                            eprintln!("Run not found: {run_id}");
                            std::process::exit(1);
                        }
                        Err(e) => {
                            eprintln!("Error: {e}");
                            std::process::exit(1);
                        }
                    }
                }
            }
        }

        Commands::DownloadSingle {
            url,
            range_start,
            range_end,
            output,
        } => {
            let range_end = range_end.unwrap_or(range_start + 4 * 1024 * 1024 - 1);
            let page_size = (cfg.defaults.page_size_kb as u64) * 1024;

            println!(
                "Downloading range {}-{} ({:.1} MB) from: {url}",
                range_start,
                range_end,
                (range_end - range_start + 1) as f64 / 1_048_576.0
            );

            let (tx, rx) = crossbeam_channel::unbounded();
            let output_path = PathBuf::from(&output);
            let writer_handle = match jsonl::spawn_jsonl_writer(rx, &output_path) {
                Ok(h) => h,
                Err(e) => {
                    eprintln!("Failed to create JSONL writer: {e}");
                    std::process::exit(1);
                }
            };

            let run_start = Instant::now();
            let params = DownloadParams {
                request_id: 1,
                chunk_id: 0,
                url: url.clone(),
                range_start,
                range_end,
                lane: Lane::Urgent,
                attempt: 1,
                page_size,
                timeout: Duration::from_secs(120),
                sink: cfg.defaults.sink,
            };

            match download::download_range(&params, &tx, run_start) {
                Ok(result) => {
                    println!();
                    println!("Download complete:");
                    println!("  Bytes:      {} ({:.1} MB)", result.total_bytes, result.total_bytes as f64 / 1_048_576.0);
                    println!("  DNS:        {:.1} ms", result.dns_ms);
                    println!("  Connect:    {:.1} ms", result.connect_ms);
                    println!("  TLS:        {:.1} ms", result.tls_ms);
                    println!("  TTFB:       {:.1} ms", result.ttfb_ms);
                    println!("  Queue:      {:.1} ms", result.queue_ms);
                    println!("  Total:      {:.1} ms", result.total_ms);
                    println!("  HTTP:       {}", result.http_version);
                    println!("  Conn ID:    {}", result.connection_id);
                    println!("  New conn:   {}", result.new_connection);
                    println!("  Response:   {}", result.response_code);
                    println!("  Remote:     {}", result.remote_endpoint);
                    println!("  Local:      {}", result.local_endpoint);
                    println!("  URL hash:   {}", result.effective_url_hash);
                    let throughput_mbps = if result.total_ms > 0.0 {
                        result.total_bytes as f64 * 8.0 / result.total_ms / 1000.0
                    } else {
                        0.0
                    };
                    println!("  Throughput: {:.1} Mbps", throughput_mbps);
                }
                Err(e) => {
                    eprintln!("Download failed: {e}");
                }
            }

            drop(tx);
            match writer_handle.join() {
                Ok(Ok(_)) => {
                    println!();
                    println!("Trace written to: {output}");
                }
                Ok(Err(e)) => eprintln!("JSONL writer error: {e}"),
                Err(_) => eprintln!("JSONL writer thread panicked"),
            }
        }
    }
}

/// Resolve a target string to a URL. If it starts with http:// or https://,
/// return it as-is. Otherwise treat it as an asset key (provider:id:...) and
/// resolve via the provider adapter.
fn resolve_target(target: &str, cfg: &config::Config) -> String {
    if target.starts_with("http://") || target.starts_with("https://") {
        return target.to_string();
    }

    let parts: Vec<&str> = target.splitn(4, ':').collect();
    if parts.len() < 2 {
        eprintln!("Invalid target. Use a URL or asset key (e.g., realdebrid:ID:FILE_INDEX:SIZE)");
        std::process::exit(1);
    }
    let provider_name = parts[0];
    let providers = build_providers(cfg, Some(provider_name));
    if providers.is_empty() {
        eprintln!("Provider '{}' not configured. Check frontier.toml or environment variables.", provider_name);
        std::process::exit(1);
    }
    let provider = &providers[0];

    println!("Resolving asset key: {target}");
    let candidates = match provider.list_candidates() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to list candidates: {e}");
            std::process::exit(1);
        }
    };
    let asset = candidates.iter().find(|a| a.key.canonical() == target).cloned();
    let asset = match asset {
        Some(a) => a,
        None => {
            eprintln!("Asset key not found: {target}");
            eprintln!("Available assets:");
            for a in &candidates {
                eprintln!("  {}", a.key.canonical());
            }
            std::process::exit(1);
        }
    };

    println!("Resolving direct URL for: {} ({:.1} MB)", asset.filename, asset.key.size_bytes as f64 / 1_048_576.0);
    let resolved = match provider.resolve_direct_url(&asset) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Failed to resolve direct URL: {e}");
            std::process::exit(1);
        }
    };
    println!("Direct URL resolved ({} bytes)", resolved.size_bytes);
    resolved.direct_url
}

fn build_providers(
    cfg: &config::Config,
    filter: Option<&str>,
) -> Vec<Box<dyn DebridProvider>> {
    let mut providers: Vec<Box<dyn DebridProvider>> = Vec::new();

    let want_rd = filter.is_none() || filter == Some("realdebrid");
    let want_pm = filter.is_none() || filter == Some("premiumize");

    if want_rd {
        if let Some(ref rd) = cfg.providers.realdebrid {
            if !rd.api_key.is_empty() {
                providers.push(Box::new(RealDebridProvider::new(rd.api_key.clone())));
            }
        }
    }
    if want_pm {
        if let Some(ref pm) = cfg.providers.premiumize {
            if !pm.api_key.is_empty() {
                providers.push(Box::new(PremiumizeProvider::new(pm.api_key.clone())));
            }
        }
    }

    providers
}
