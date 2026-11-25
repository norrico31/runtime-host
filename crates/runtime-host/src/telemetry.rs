
use chrono::{SecondsFormat, Utc};
use serde::Serialize;
use serde_json::Value;
use std::{
    fs::{File, OpenOptions},
    io::{self, Write},
    path::Path,
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    },
};

#[derive(Debug, Serialize)]
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
        if let Some(parent) = path.as_ref().parent() {
            std::fs::create_dir_all(parent)?;
        }

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;

        Ok(Self {
            file: Mutex::new(file),
            event_sequence: AtomicU64::new(1),
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
        let sequence = self.event_sequence.fetch_add(1, Ordering::Relaxed);

        let event = ObservationEvent {
            event_id: format!("evt-{sequence:06}"),
            timestamp: Utc::now()
                .to_rfc3339_opts(SecondsFormat::Micros, true),
            event_type: event_type.to_string(),
            instance_id: instance_id.map(str::to_owned),
            request_id: request_id.map(str::to_owned),
            component: component.to_string(),
            attributes,
        };

        let Ok(mut file) = self.file.lock() else {
            eprintln!("telemetry error: event log lock poisoned");
            return;
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

