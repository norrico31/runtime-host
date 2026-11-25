use std::{fs::File, path::PathBuf};

use wasmtime::{Engine, component::Component};

use crate::config::RuntimeConfig;

pub struct PreflightedService {
    pub service_name: String,
    pub component_path: PathBuf,
    pub port: u16,
    pub component: Component,
}

pub fn preflight_components(
    engine: &Engine,
    config: &RuntimeConfig,
) -> wasmtime::Result<Vec<PreflightedService>> {
    println!("Starting component artifact preflight...");

    let mut preflighted = Vec::new();

    for (service_name, service_config) in &config.services {
        if !service_config.enabled {
            println!("Skipping disabled service: {}", service_name);
            continue;
        }

        let component_path = &service_config.component;

        println!(
            "Checking component artifact: service={} path={}",
            service_name,
            component_path.display()
        );

        // Check whether the artifact exists and is accessible.
        let metadata = std::fs::metadata(component_path).map_err(|err| {
            wasmtime::Error::msg(format!(
                "Artifact preflight failed for service '{}': \
                     cannot access '{}': {}",
                service_name,
                component_path.display(),
                err
            ))
        })?;

        // Ensure the artifact points to a regular file.
        if !metadata.is_file() {
            return Err(wasmtime::Error::msg(format!(
                "Artifact preflight failed for service '{}': \
                 component path is not a regular file: {}",
                service_name,
                component_path.display()
            )));
        }

        // Reject empty artifacts.
        if metadata.len() == 0 {
            return Err(wasmtime::Error::msg(format!(
                "Artifact preflight failed for service '{}': \
                 component artifact is empty: {}",
                service_name,
                component_path.display()
            )));
        }

        // Verify that the artifact can be opened for reading.
        File::open(component_path).map_err(|err| {
            wasmtime::Error::msg(format!(
                "Artifact preflight failed for service '{}': \
                 cannot read '{}': {}",
                service_name,
                component_path.display(),
                err
            ))
        })?;

        // Validate and compile the WASM component using Wasmtime.
        let component = Component::from_file(engine, component_path).map_err(|err| {
            wasmtime::Error::msg(format!(
                "Artifact preflight failed for service '{}': \
                 invalid or unsupported WASM component '{}': {}",
                service_name,
                component_path.display(),
                err
            ))
        })?;

        println!(
            "Artifact preflight passed: service={} size={} bytes",
            service_name,
            metadata.len()
        );

        preflighted.push(PreflightedService {
            service_name: service_name.clone(),
            component_path: component_path.clone(),
            port: service_config.port,
            component,
        });
    }

    if preflighted.is_empty() {
        return Err(wasmtime::Error::msg(
            "Artifact preflight failed: no enabled components available",
        ));
    }

    println!(
        "Component artifact preflight completed successfully: \
         {} component(s) validated",
        preflighted.len()
    );

    Ok(preflighted)
}
