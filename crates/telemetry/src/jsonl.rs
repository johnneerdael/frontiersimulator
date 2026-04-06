use crate::TraceEvent;
use crossbeam_channel::Receiver;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::Path;
use std::thread;

/// Spawns a background thread that reads TraceEvents from the channel
/// and writes them as newline-delimited JSON to the given file path.
///
/// Returns a JoinHandle that resolves when the channel is closed
/// and all events have been flushed.
pub fn spawn_jsonl_writer(
    rx: Receiver<TraceEvent>,
    output_path: &Path,
) -> Result<thread::JoinHandle<std::io::Result<usize>>, std::io::Error> {
    // Ensure parent directory exists
    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let file = File::create(output_path)?;
    let path_display = output_path.display().to_string();

    Ok(thread::spawn(move || {
        let mut writer = BufWriter::new(file);
        let mut count = 0usize;

        for event in rx {
            serde_json::to_writer(&mut writer, &event)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
            writer.write_all(b"\n")?;
            count += 1;
        }

        writer.flush()?;
        eprintln!("JSONL writer: wrote {count} events to {path_display}");
        Ok(count)
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{Lane, TraceEvent};
    use std::io::BufRead;

    #[test]
    fn writes_events_as_jsonl() {
        let dir = std::env::temp_dir().join("frontier_test_jsonl");
        let path = dir.join("test_trace.jsonl");

        let (tx, rx) = crossbeam_channel::unbounded();

        let handle = spawn_jsonl_writer(rx, &path).unwrap();

        // Send some events
        tx.send(TraceEvent::RequestStarted {
            request_id: 1,
            chunk_id: 0,
            lane: Lane::Urgent,
            attempt: 1,
            requested_range: "0-131071".to_string(),
            timestamp_ns: 1000,
        })
        .unwrap();

        tx.send(TraceEvent::PageCompleted {
            request_id: 1,
            chunk_id: 0,
            page_index: 0,
            page_start_byte: 0,
            page_end_byte: 131072,
            page_bytes: 131072,
            cumulative_bytes: 131072,
            timestamp_ns: 2000,
        })
        .unwrap();

        drop(tx); // Close channel
        let result = handle.join().unwrap().unwrap();
        assert_eq!(result, 2);

        // Verify JSONL is valid
        let file = File::open(&path).unwrap();
        let reader = std::io::BufReader::new(file);
        let lines: Vec<String> = reader.lines().map(|l| l.unwrap()).collect();
        assert_eq!(lines.len(), 2);

        // Verify each line is valid JSON
        for line in &lines {
            let _: serde_json::Value = serde_json::from_str(line).unwrap();
        }

        // Verify first event type
        let first: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(first["event_type"], "request_started");
        assert_eq!(first["request_id"], 1);

        // Cleanup
        let _ = fs::remove_dir_all(&dir);
    }
}
