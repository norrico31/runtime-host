mod artifact;
mod config;
mod gateway;
mod instance;
mod network;
mod telemetry;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Response, body::Incoming, server::conn::http1};
use serde_json::json;

use std::{
    io::Read,
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use instance::InstancePool;
use telemetry::Telemetry;

use tokio::net::TcpListener;

use wasmtime::{Engine, Result, component::Linker};

use wasmtime_wasi_http::{
    WasiHttpView,
    io::TokioIo,
    p2::{
        bindings::{
            ProxyPre,
            http::types::Scheme,
        },
    },
};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const EPOCH_TICK: Duration = Duration::from_millis(100);

type HostResponse = Response<Full<Bytes>>;

fn classify_runtime_error(err: &wasmtime::Error) -> &'static str {
    match err.downcast_ref::<wasmtime::Trap>() {
        Some(wasmtime::Trap::Interrupt) => "runtime.epoch_interrupted",
        Some(wasmtime::Trap::OutOfFuel) => "runtime.fuel_exhausted",
        Some(_) => "runtime.trap",
        None => "runtime.error",
    }
}

struct Server {
    pool: Arc<InstancePool>,
    requests: AtomicU64,
    request_sequence: AtomicU64,
    telemetry: Arc<Telemetry>,
    component: String,
    internal_token: Arc<String>,
}

impl Server {
    async fn handle(
        &self,
        req: hyper::Request<Incoming>,
        peer_addr: SocketAddr,
        local_addr: SocketAddr,
    ) -> Result<HostResponse> {
        // Only the gateway may access internal workload listeners.
        let provided_token = req
            .headers()
            .get("x-runtime-internal-token")
            .and_then(|value| value.to_str().ok());

        if provided_token != Some(self.internal_token.as_str()) {
            return Ok(Response::builder()
                .status(hyper::StatusCode::FORBIDDEN)
                .body(Full::new(Bytes::new()))
                .expect("valid 403 response"));
        }

        let started = Instant::now();
        let method = req.method().to_string();
        let path = req.uri().path().to_string();

        let request_number = self.request_sequence.fetch_add(1, Ordering::Relaxed) + 1;

        let request_id = format!("{}-req-{:06}", self.component, request_number);

        let Some(slot) = self.pool.select() else {
            self.telemetry.record(
                "request.failed",
                &self.component,
                None,
                Some(&request_id),
                json!({
                    "method": method,
                    "path": path,
                    "reason": "no_healthy_instances"
                }),
            );

            return Ok(Response::builder()
                .status(hyper::StatusCode::SERVICE_UNAVAILABLE)
                .body(Full::new(Bytes::new()))
                .expect("valid 503 response"));
        };

        let instance_id = slot.id.clone();

        self.telemetry.record(
            "request.started",
            &self.component,
            Some(&instance_id),
            Some(&request_id),
            json!({
                "method": method,
                "path": path,
                "source_ip": peer_addr.ip().to_string(),
                "source_port": peer_addr.port(),
                "destination_ip": local_addr.ip().to_string(),
                "destination_port": local_addr.port()
            }),
        );

        // Lock one persistent instance. Requests assigned to the same
        // instance are serialized; other instances remain available.
        let mut runtime = Arc::clone(&slot.runtime).lock_owned().await;

        // Reset per-request execution budgets.
        runtime
            .store
            .set_epoch_deadline(instance::EPOCH_DEADLINE_TICKS as u64);

        runtime.store.set_fuel(instance::WASM_FUEL_BUDGET)?;

        // Create request resources inside the EXISTING Store.
        let incoming = runtime
            .store
            .data_mut()
            .http()
            .new_incoming_request(Scheme::Http, req)?;

        let (sender, receiver) = tokio::sync::oneshot::channel();

        let out = runtime
            .store
            .data_mut()
            .http()
            .new_response_outparam(sender)?;

        let task = tokio::spawn(async move {
            // Split mutable and immutable borrows of RuntimeInstance.
            let runtime = &mut *runtime;

            let store = &mut runtime.store;
            let proxy = &runtime.proxy;

            // Execute using the existing persistent Store and Proxy.
            proxy
                .wasi_http_incoming_handler()
                .call_handle(store, incoming, out)
                .await
        });
        let abort_handle = task.abort_handle();

        let result = tokio::time::timeout(REQUEST_TIMEOUT, async move {
            let response = match receiver.await {
                Ok(Ok(response)) => response,

                Ok(Err(err)) => {
                    let _ = task.await;

                    return Err(wasmtime::Error::msg(format!(
                        "Guest HTTP response error: {err:?}"
                    )));
                }

                Err(_) => {
                    let message = match task.await {
                        Ok(Ok(())) => "Guest finished without setting an HTTP response".to_string(),
                        Ok(Err(err)) => err.to_string(),
                        Err(err) => format!("Guest task failed: {err}"),
                    };

                    return Err(wasmtime::Error::msg(message));
                }
            };

            // Consume the body while the persistent Store is still
            // exclusively owned by this request.
            let (parts, body) = response.into_parts();

            let collected = match body.collect().await {
                Ok(body) => body,

                Err(err) => {
                    task.abort();
                    let _ = task.await;

                    return Err(wasmtime::Error::msg(format!(
                        "Failed to collect guest response body: {err}"
                    )));
                }
            };

            match task.await {
                Ok(Ok(())) => {}

                Ok(Err(err)) => return Err(err),

                Err(err) => {
                    return Err(wasmtime::Error::msg(format!(
                        "Guest task join error: {err}"
                    )));
                }
            }

            Ok(Response::from_parts(parts, Full::new(collected.to_bytes())))
        })
        .await;

        match result {
            Ok(Ok(response)) => {
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

                Ok(response)
            }

            Ok(Err(err)) => {
                let event_type = classify_runtime_error(&err);
                let message = err.to_string();

                slot.mark_unhealthy();

                self.telemetry.record(
                    event_type,
                    &self.component,
                    Some(&instance_id),
                    Some(&request_id),
                    json!({
                        "error": message,
                        "instance_health": "unhealthy"
                    }),
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

                eprintln!("Instance {} marked unhealthy: {}", instance_id, message);

                Ok(Response::builder()
                    .status(hyper::StatusCode::INTERNAL_SERVER_ERROR)
                    .body(Full::new(Bytes::new()))
                    .expect("valid 500 response"))
            }

            Err(_) => {
                // Do not reuse an instance whose execution was cancelled
                // by the request timeout.
                abort_handle.abort();
                slot.mark_unhealthy();

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
                        "reason": "wall_clock_timeout",
                        "instance_health": "unhealthy"
                    }),
                );

                eprintln!(
                    "Instance {} marked unhealthy after request timeout.",
                    instance_id
                );

                Ok(Response::builder()
                    .status(hyper::StatusCode::GATEWAY_TIMEOUT)
                    .body(Full::new(Bytes::new()))
                    .expect("valid 504 response"))
            }
        }
    }
}

async fn serve_listener(listener: TcpListener, server: Arc<Server>) -> Result<()> {
    loop {
        let local_addr = listener.local_addr()?;
        let (stream, peer_addr) = listener.accept().await?;
        let server = Arc::clone(&server);

        tokio::spawn(async move {
            let service = hyper::service::service_fn(move |req| {
                let server = Arc::clone(&server);

                async move {
                    let response = match server.handle(req, peer_addr, local_addr).await {
                        Ok(response) => response,

                        Err(err) => {
                            eprintln!("component request failed: {err:#}");

                            Response::builder()
                                .status(hyper::StatusCode::INTERNAL_SERVER_ERROR)
                                .body(Full::new(Bytes::new()))
                                .expect("valid 500 response")
                        }
                    };

                    Ok::<_, std::io::Error>(response)
                }
            });

            if let Err(err) = http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .await
            {
                eprintln!("connection {peer_addr} failed: {err}");
            }
        });
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "wasm-services.toml".to_string());

    println!("Loading runtime configuration: {}", config_path);

    let runtime_config = config::RuntimeConfig::load(&config_path)
        .map_err(|err| wasmtime::Error::msg(format!("Failed to load configuration: {err}")))?;

    std::fs::create_dir_all("telemetry")?;

    let telemetry = Arc::new(Telemetry::open("telemetry/events.jsonl")?);

    // Initialize internal gateway access token.
    let mut token_bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut token_bytes)?;

    let internal_token = Arc::new(
        token_bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>(),
    );

    println!("Internal service access token initialized.");

    let mut wasmtime_config = wasmtime::Config::new();
    wasmtime_config.consume_fuel(true);
    wasmtime_config.epoch_interruption(true);

    let engine = Engine::new(&wasmtime_config)?;

    println!("Wasmtime engine initialized successfully.");

    // PHASE 1: HOST-MANAGED ARTIFACT BUILD
    artifact::build_configured_artifacts(&runtime_config)?;

    // PHASE 2: COMPONENT ARTIFACT PREFLIGHT
    let preflighted_workloads = artifact::preflight_components(&engine, &runtime_config)?;

    println!(
        "Artifact preflight passed for {} workload(s).",
        preflighted_workloads.len()
    );

    // PHASE 3: COMPONENT LINKER PREFLIGHT
    let mut prepared_workloads = Vec::new();

    for workload in preflighted_workloads {
        println!(
            "Checking component linker compatibility: workload={}",
            workload.workload_name
        );

        let mut linker = Linker::new(&engine);

        wasmtime_wasi::p2::add_to_linker_async(&mut linker).map_err(|err| {
            wasmtime::Error::msg(format!(
                "WASI linker setup failed for '{}': {}",
                workload.workload_name, err
            ))
        })?;

        wasmtime_wasi_http::p2::add_only_http_to_linker_async(&mut linker).map_err(|err| {
            wasmtime::Error::msg(format!(
                "WASI HTTP linker setup failed for '{}': {}",
                workload.workload_name, err
            ))
        })?;

        let instance_pre = linker.instantiate_pre(&workload.component).map_err(|err| {
            wasmtime::Error::msg(format!(
                "Component linker preflight failed for '{}': {}",
                workload.workload_name, err
            ))
        })?;

        let pre = ProxyPre::new(instance_pre).map_err(|err| {
            wasmtime::Error::msg(format!(
                "Proxy pre-instantiation failed for '{}': {}",
                workload.workload_name, err
            ))
        })?;

        prepared_workloads.push((
            workload.workload_name,
            workload.artifact_path,
            workload.instances,
            pre,
        ));
    }

    println!(
        "Component linker preflight completed: {} workload(s).",
        prepared_workloads.len()
    );

    // PHASE 4: CREATE PERSISTENT INSTANCE POOLS AND LISTENERS
    let mut services = Vec::new();
    let mut service_registry = network::ServiceRegistry::new();

    for (workload_name, artifact_path, instance_count, pre) in prepared_workloads {
        println!(
            "Initializing workload '{}' with {} persistent instance(s).",
            workload_name, instance_count
        );

        let pool = Arc::new(
            InstancePool::create(&pre, &workload_name, instance_count, Arc::clone(&telemetry))
                .await?,
        );

        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;

        let server = Arc::new(Server {
            pool: Arc::clone(&pool),
            requests: AtomicU64::new(0),
            request_sequence: AtomicU64::new(1),
            telemetry: Arc::clone(&telemetry),
            component: workload_name.clone(),
            internal_token: Arc::clone(&internal_token),
        });

        service_registry
            .register(workload_name.clone(), address)
            .map_err(|err| wasmtime::Error::msg(format!("Workload registration failed: {err}")))?;

        telemetry.record(
            "runtime.started",
            &workload_name,
            None,
            None,
            json!({
                "host": "runtime-host",
                "wasmtime_version": "50",
                "artifact_path": artifact_path.display().to_string(),
                "instance_count": pool.len(),
                "listener_address": address.to_string()
            }),
        );

        println!(
            "Workload '{}' ready: listener={} instances={}",
            workload_name,
            address,
            pool.len()
        );

        services.push((workload_name, listener, server));
    }

    let service_registry = Arc::new(service_registry);

    println!(
        "Internal service registry initialized: {} workload(s).",
        service_registry.len()
    );

    // PHASE 5: DEFAULT GATEWAY
    let gateway_listener = if let Some(gateway_config) = &runtime_config.gateway {
        if gateway_config.enabled {
            let gateway =
                gateway::Gateway::new(Arc::clone(&service_registry), Arc::clone(&internal_token))?;

            let listener =
                TcpListener::bind((gateway_config.host.as_str(), gateway_config.port)).await?;

            println!(
                "Default gateway listening on http://{}:{}",
                gateway_config.host, gateway_config.port
            );

            Some((gateway, listener))
        } else {
            println!("Default gateway is disabled.");
            None
        }
    } else {
        println!("Gateway configuration not found. Gateway disabled.");
        None
    };

    // PHASE 6: WASMTIME EPOCH TICKER
    let epoch_engine = engine.clone();

    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(EPOCH_TICK);

        loop {
            ticker.tick().await;
            epoch_engine.increment_epoch();
        }
    });

    // PHASE 7: START INTERNAL WORKLOAD LISTENERS
    for (workload_name, listener, server) in services {
        tokio::spawn(async move {
            if let Err(err) = serve_listener(listener, server).await {
                eprintln!("Workload '{}' listener stopped: {err:#}", workload_name);
            }
        });
    }

    // PHASE 8: START DEFAULT GATEWAY
    if let Some((gateway, listener)) = gateway_listener {
        tokio::spawn(async move {
            if let Err(err) = gateway::serve(gateway, listener).await {
                eprintln!("Default gateway stopped: {err:#}");
            }
        });
    }

    let active_workloads = runtime_config
        .workloads
        .values()
        .filter(|workload| workload.enabled)
        .count();

    let total_instances: usize = runtime_config
        .workloads
        .values()
        .filter(|workload| workload.enabled)
        .map(|workload| workload.instances)
        .sum();

    println!("==========================================");
    println!("Runtime host initialized successfully.");
    println!("Active workloads: {}", active_workloads);
    println!("Persistent instances: {}", total_instances);
    println!("==========================================");

    std::future::pending::<()>().await;

    Ok(())
}
