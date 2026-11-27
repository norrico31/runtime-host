use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use serde_json::json;
use tokio::sync::Mutex;

use wasmtime::component::ResourceTable;
use wasmtime::{Engine, Store};

use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};
use wasmtime_wasi_http::{
    WasiHttpCtx, WasiHttpCtxView, WasiHttpView,
    p2::bindings::{Proxy, ProxyPre},
};

use crate::telemetry::Telemetry;

pub const WASM_FUEL_BUDGET: u64 = 1_000_000_000;
pub const EPOCH_DEADLINE_TICKS: u8 = 40;

pub struct HostState {
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

/// One persistent Wasmtime execution environment.
pub struct RuntimeInstance {
    pub store: Store<HostState>,
    pub proxy: Proxy,
}

/// A slot represents one independently managed instance.
pub struct InstanceSlot {
    pub id: String,
    pub runtime: Arc<Mutex<RuntimeInstance>>,
    healthy: AtomicBool,
}

impl InstanceSlot {
    pub fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Acquire)
    }

    pub fn mark_unhealthy(&self) {
        self.healthy.store(false, Ordering::Release);
    }
}

/// A workload owns a pool of persistent instances.
pub struct InstancePool {
    slots: Vec<Arc<InstanceSlot>>,
    cursor: AtomicUsize,
}

impl InstancePool {
    pub async fn create(
        pre: &ProxyPre<HostState>,
        workload_name: &str,
        instance_count: usize,
        telemetry: Arc<Telemetry>,
    ) -> wasmtime::Result<Self> {
        let mut slots = Vec::with_capacity(instance_count);

        for index in 0..instance_count {
            let instance_id = format!("{}-inst-{:03}", workload_name, index + 1);

            telemetry.record(
                "instance.created",
                workload_name,
                Some(&instance_id),
                None,
                json!({
                    "creation_reason": "runtime_startup",
                    "instance_index": index + 1
                }),
            );

            let mut store = Store::new(
                pre.engine(),
                HostState {
                    table: ResourceTable::new(),
                    wasi: WasiCtx::builder().inherit_stdio().build(),
                    http: WasiHttpCtx::new(),
                },
            );

            store.set_epoch_deadline(EPOCH_DEADLINE_TICKS as u64);
            store.set_fuel(WASM_FUEL_BUDGET)?;

            // Instantiate exactly once during startup.
            let proxy = pre.instantiate_async(&mut store).await?;

            telemetry.record(
                "instance.ready",
                workload_name,
                Some(&instance_id),
                None,
                json!({
                    "lifecycle": "persistent",
                    "instance_index": index + 1
                }),
            );

            let runtime = RuntimeInstance { store, proxy };

            slots.push(Arc::new(InstanceSlot {
                id: instance_id,
                runtime: Arc::new(Mutex::new(runtime)),
                healthy: AtomicBool::new(true),
            }));
        }

        Ok(Self {
            slots,
            cursor: AtomicUsize::new(0),
        })
    }

    /// Round-robin selection among healthy instances.
    pub fn select(&self) -> Option<Arc<InstanceSlot>> {
        if self.slots.is_empty() {
            return None;
        }

        let start = self.cursor.fetch_add(1, Ordering::Relaxed);

        for offset in 0..self.slots.len() {
            let index = (start + offset) % self.slots.len();
            let slot = &self.slots[index];

            if slot.is_healthy() {
                return Some(Arc::clone(slot));
            }
        }

        None
    }

    pub fn len(&self) -> usize {
        self.slots.len()
    }
}
