use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fs::{File, OpenOptions},
    io::{self, BufRead, BufReader, Write},
    path::Path,
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

#[derive(Debug, Serialize, Deserialize)]
pub struct ObservationEvent {
    pub event_id: String,
    pub timestamp: String,
    pub event_type: String,
    pub instance_id: Option<String>,
    pub request_id: Option<String>,
    pub component: String,
    pub attributes: Value,
}

pub struct Telemetry {
    file: Mutex<File>,
    event_sequence: AtomicU64,
}

impl Telemetry {
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref();

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let next_sequence = Self::next_event_sequence(path)?;

        let file = OpenOptions::new().create(true).append(true).open(path)?;

        Ok(Self {
            file: Mutex::new(file),
            event_sequence: AtomicU64::new(next_sequence),
        })
    }

    fn next_event_sequence(path: &Path) -> io::Result<u64> {
        if !path.exists() {
            return Ok(1);
        }

        let file = File::open(path)?;
        let reader = BufReader::new(file);

        let mut max_sequence = 0u64;

        for (index, line) in reader.lines().enumerate() {
            let line = line?;

            if line.trim().is_empty() {
                continue;
            }

            let event: ObservationEvent = serde_json::from_str(&line).map_err(|err| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("Invalid telemetry JSON at line {}: {}", index + 1, err),
                )
            })?;

            let sequence = event
                .event_id
                .strip_prefix("evt-")
                .and_then(|value| value.parse::<u64>().ok())
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "Invalid telemetry event ID at line {}: {}",
                            index + 1,
                            event.event_id
                        ),
                    )
                })?;

            max_sequence = max_sequence.max(sequence);
        }

        max_sequence.checked_add(1).ok_or_else(|| {
            io::Error::new(io::ErrorKind::Other, "Telemetry event sequence exhausted")
        })
    }

    pub fn record(
        &self,
        event_type: &str,
        component: &str,
        instance_id: Option<&str>,
        request_id: Option<&str>,
        attributes: Value,
    ) {
        let Ok(mut file) = self.file.lock() else {
            eprintln!("telemetry error: event log lock poisoned");
            return;
        };

        let sequence = self.event_sequence.fetch_add(1, Ordering::Relaxed);

        let event = ObservationEvent {
            event_id: format!("evt-{sequence:06}"),
            timestamp: Utc::now().to_rfc3339_opts(SecondsFormat::Micros, true),
            event_type: event_type.to_string(),
            instance_id: instance_id.map(str::to_owned),
            request_id: request_id.map(str::to_owned),
            component: component.to_string(),
            attributes,
        };

        if let Err(err) = serde_json::to_writer(&mut *file, &event) {
            eprintln!("telemetry serialization error: {err}");
            return;
        }

        if let Err(err) = file.write_all(b"\n") {
            eprintln!("telemetry write error: {err}");
            return;
        }

        if let Err(err) = file.flush() {
            eprintln!("telemetry flush error: {err}");
        }
    }
}
