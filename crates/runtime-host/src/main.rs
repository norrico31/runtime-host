mod telemetry;

use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use hyper::server::conn::http1;
use serde_json::json;
use telemetry::Telemetry;
use tokio::net::TcpListener;
use uuid::Uuid;
use wasmtime::{
    component::{Component, Linker, ResourceTable},
    Engine, Result, Store,
};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};
use wasmtime_wasi_http::{
    io::TokioIo,
    p2::{
        bindings::{http::types::{ErrorCode, Scheme}, ProxyPre},
        body::HyperOutgoingBody,
    },
    WasiHttpCtx, WasiHttpCtxView, WasiHttpView,
};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const EPOCH_TICK: Duration = Duration::from_millis(100);
const WASM_FUEL_BUDGET: u64 = 1_000_000_000;
const EPOCH_DEADLINE_TICKS: u8 = 40;

fn classify_runtime_error(err: &wasmtime::Error) -> &'static str {
    match err.downcast_ref::<wasmtime::Trap>() {
        Some(wasmtime::Trap::OutOfFuel) | Some(wasmtime::Trap::Interrupt) => {
            "runtime.interrupted"
        }
        Some(_) => "runtime.trap",
        None => "runtime.error",
    }
}

struct HostState {
    table: ResourceTable,
    wasi: WasiCtx,
    http: WasiHttpCtx,
}

impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl WasiHttpView for HostState {
    fn http(&mut self) -> WasiHttpCtxView<'_> {
        WasiHttpCtxView {
            ctx: &mut self.http,
            table: &mut self.table,
            hooks: Default::default(),
        }
    }
}

struct Server {
    pre: ProxyPre<HostState>,
    requests: AtomicU64,
    telemetry: Arc<Telemetry>,
    component: String,
}

impl Server {
    async fn handle(
        &self,
        req: hyper::Request<hyper::body::Incoming>,
    ) -> Result<hyper::Response<HyperOutgoingBody>> {
        let started = Instant::now();
        let method = req.method().to_string();
        let path = req.uri().path().to_string();
        let request_id = Uuid::new_v4().to_string();
        let instance_id = Uuid::new_v4().to_string();

        self.telemetry.record(
            "request.started",
            &self.component,
            Some(&instance_id),
            Some(&request_id),
            json!({ "method": method, "path": path }),
        );
        self.telemetry.record(
            "instance.created",
            &self.component,
            Some(&instance_id),
            Some(&request_id),
            json!({ "creation_reason": "http_request" }),
        );

        let mut store = Store::new(
            self.pre.engine(),
            HostState {
                table: ResourceTable::new(),
                wasi: WasiCtx::builder().inherit_stdio().build(),
                http: WasiHttpCtx::new(),
            },
        );
        store.set_fuel(WASM_FUEL_BUDGET)?;
        store.set_epoch_deadline(EPOCH_DEADLINE_TICKS as u64);

        let (sender, receiver) = tokio::sync::oneshot::channel();
        let incoming = store
            .data_mut()
            .http()
            .new_incoming_request(Scheme::Http, req)?;
        let out = store.data_mut().http().new_response_outparam(sender)?;

        let pre = self.pre.clone();
        let telemetry = Arc::clone(&self.telemetry);
        let component = self.component.clone();
        let task_instance_id = instance_id.clone();
        let task_request_id = request_id.clone();

        let task = tokio::spawn(async move {
            let proxy = pre.instantiate_async(&mut store).await?;
            telemetry.record(
                "instance.ready",
                &component,
                Some(&task_instance_id),
                Some(&task_request_id),
                json!({}),
            );
            proxy
                .wasi_http_incoming_handler()
                .call_handle(store, incoming, out)
                .await?;
            Ok::<(), wasmtime::Error>(())
        });

        let received = tokio::time::timeout(REQUEST_TIMEOUT, receiver).await;

        match received {
            Ok(Ok(Ok(response))) => {
                let count = self.requests.fetch_add(1, Ordering::Relaxed) + 1;
                let status = response.status().as_u16();
                let duration_us = started.elapsed().as_micros();

                println!(
                    "host_observation request_count={} request_id={} instance_id={} method={} path={} status={} duration_us={}",
                    count, request_id, instance_id, method, path, status, duration_us
                );

                self.telemetry.record(
                    "request.completed",
                    &self.component,
                    Some(&instance_id),
                    Some(&request_id),
                    json!({
                        "method": method,
                        "path": path,
                        "status": status,
                        "duration_us": duration_us,
                        "outcome": if status >= 500 {
                            "server_error"
                        } else if status >= 400 {
                            "client_error"
                        } else {
                            "success"
                        }
                    }),
                );

                // Response may stream after headers. Watch guest completion separately.
                let telemetry = Arc::clone(&self.telemetry);
                let component = self.component.clone();
                let watcher_instance_id = instance_id.clone();
                let watcher_request_id = request_id.clone();

                tokio::spawn(async move {
                    match task.await {
                        Ok(Ok(())) => {}
                        Ok(Err(err)) => {
                            let event_type = classify_runtime_error(&err);
                            telemetry.record(
                                event_type,
                                &component,
                                Some(&watcher_instance_id),
                                Some(&watcher_request_id),
                                json!({ "error": err.to_string() }),
                            );
                        }
                        Err(err) => {
                            telemetry.record(
                                "runtime.error",
                                &component,
                                Some(&watcher_instance_id),
                                Some(&watcher_request_id),
                                json!({
                                    "error": err.to_string(),
                                    "kind": "task_join_error"
                                }),
                            );
                        }
                    }
                    telemetry.record(
                        "instance.terminated",
                        &component,
                        Some(&watcher_instance_id),
                        Some(&watcher_request_id),
                        json!({ "reason": "request_task_finished" }),
                    );
                });

                Ok(response)
            }

            // A response was sent through the WASI HTTP outparam as an error.
            Ok(Ok(Err(err))) => {
                let message = format!("{err:?}");
                let event_type = classify_http_error_code(&err);

                self.telemetry.record(
                    "request.failed",
                    &self.component,
                    Some(&instance_id),
                    Some(&request_id),
                    json!({
                        "method": method,
                        "path": path,
                        "error": message
                    }),
                );
                self.telemetry.record(
                    event_type,
                    &self.component,
                    Some(&instance_id),
                    Some(&request_id),
                    json!({ "error": message }),
                );

                let _ = task.await;
                self.telemetry.record(
                    "instance.terminated",
                    &self.component,
                    Some(&instance_id),
                    Some(&request_id),
                    json!({ "reason": "response_error" }),
                );

                Err(wasmtime::Error::msg(format!("Guest HTTP error: {err:?}")))
            }

            // Guest ended without delivering a response.
            Ok(Err(_recv_error)) => {
                let (event_type, message) = match task.await {
                    Ok(Ok(())) => (
                        "runtime.error",
                        "Guest finished without setting an HTTP response".to_string(),
                    ),
                    Ok(Err(err)) => (classify_runtime_error(&err), err.to_string()),
                    Err(err) => ("runtime.error", format!("Guest task failed: {err}")),
                };

                self.telemetry.record(
                    event_type,
                    &self.component,
                    Some(&instance_id),
                    Some(&request_id),
                    json!({ "error": message }),
                );
                self.telemetry.record(
                    "request.failed",
                    &self.component,
                    Some(&instance_id),
                    Some(&request_id),
                    json!({
                        "method": method,
                        "path": path,
                        "error": message
                    }),
                );
                self.telemetry.record(
                    "instance.terminated",
                    &self.component,
                    Some(&instance_id),
                    Some(&request_id),
                    json!({ "reason": "guest_task_failed" }),
                );

                Err(wasmtime::Error::msg(message))
            }

            // Tokio's wall-clock response timeout expired.
            Err(_elapsed) => {
                task.abort();
                let _ = task.await;
                let duration_us = started.elapsed().as_micros();

                self.telemetry.record(
                    "request.timeout",
                    &self.component,
                    Some(&instance_id),
                    Some(&request_id),
                    json!({
                        "method": method,
                        "path": path,
                        "timeout_ms": REQUEST_TIMEOUT.as_millis(),
                        "duration_us": duration_us,
                        "reason": "wall_clock_timeout"
                    }),
                );
                self.telemetry.record(
                    "instance.terminated",
                    &self.component,
                    Some(&instance_id),
                    Some(&request_id),
                    json!({ "reason": "request_timeout" }),
                );

                Err(wasmtime::Error::msg("WASM request timed out"))
            }
        }
    }
}

/// The WASI HTTP outparam error is an HTTP/WASI ErrorCode, not a Wasmtime trap.
/// Keep it separate from guest execution traps.
fn classify_http_error_code(_err: &ErrorCode) -> &'static str {
    "runtime.error"
}

#[tokio::main]
async fn main() -> Result<()> {
    let path = std::env::args().nth(1).unwrap_or_else(|| {
        "../sample-wasi-http-rust/target/wasm32-wasip2/release/sample_wasi_http_rust.wasm"
            .to_string()
    });
    let component_name = std::path::Path::new(&path)
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("wasm-component")
        .to_string();

    std::fs::create_dir_all("telemetry")?;
    let telemetry = Arc::new(Telemetry::open("telemetry/events.jsonl")?);

    let mut config = wasmtime::Config::new();
    config.consume_fuel(true);
    config.epoch_interruption(true);

    let engine = Engine::new(&config)?;
    let component = Component::from_file(&engine, &path)?;
    let mut linker = Linker::new(&engine);

    wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
    wasmtime_wasi_http::p2::add_only_http_to_linker_async(&mut linker)?;

    let pre = ProxyPre::new(linker.instantiate_pre(&component)?)?;

    telemetry.record(
        "runtime.started",
        &component_name,
        None,
        None,
        json!({
            "host": "runtime-host",
            "wasmtime_version": "50",
            "component_path": path
        }),
    );

    let server = Arc::new(Server {
        pre,
        requests: AtomicU64::new(0),
        telemetry: Arc::clone(&telemetry),
        component: component_name.clone(),
    });

    let epoch_engine = engine.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(EPOCH_TICK);
        loop {
            ticker.tick().await;
            epoch_engine.increment_epoch();
        }
    });

    let listener = TcpListener::bind("127.0.0.1:8081").await?;
    println!("WASM HTTP host listening on http://127.0.0.1:8081");
    println!("Component: {path}");
    println!("Telemetry: telemetry/events.jsonl");

    loop {
        let (stream, addr) = listener.accept().await?;
        let server = Arc::clone(&server);

        tokio::spawn(async move {
            let service = hyper::service::service_fn(move |req| {
                let server = Arc::clone(&server);
                async move {
                    match server.handle(req).await {
                        Ok(response) => Ok::<_, std::io::Error>(response),
                        Err(err) => {
                            eprintln!("component request failed: {err:#}");
                            let response = hyper::Response::builder()
                                .status(hyper::StatusCode::INTERNAL_SERVER_ERROR)
                                .body(HyperOutgoingBody::default())
                                .expect("valid 500 response");
                            Ok(response)
                        }
                    }
                }
            });

            if let Err(err) = http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .await
            {
                eprintln!("connection {addr} failed: {err}");
            }
        });
    }
}

