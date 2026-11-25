use std::{
    fs::{self, File},
    path::PathBuf,
    process::Command,
};

use wasmtime::{Engine, component::Component};

use crate::config::RuntimeConfig;

pub struct PreflightedService {
    pub service_name: String,
    pub component_path: PathBuf,
    pub port: u16,
    pub component: Component,
}

/// Build all enabled services that have a [build] configuration.
/// Services without a build configuration continue using their existing artifact.
pub fn build_configured_artifacts(config: &RuntimeConfig) -> wasmtime::Result<()> {
    println!("Starting host-managed artifact build pipeline...");

    for (service_name, service) in &config.services {
        if !service.enabled {
            println!("Skipping build for disabled service: {}", service_name);
            continue;
        }

        let Some(build) = &service.build else {
            println!(
                "No build configuration for service '{}'; using existing artifact.",
                service_name
            );
            continue;
        };

        println!(
            "Building service: name={} manifest={}",
            service_name,
            build.manifest.display()
        );

        if !build.manifest.is_file() {
            return Err(wasmtime::Error::msg(format!(
                "Build failed for service '{}': Cargo manifest does not exist: {}",
                service_name,
                build.manifest.display()
            )));
        }

        let mut command = Command::new("cargo");

        command
            .arg("build")
            .arg("--manifest-path")
            .arg(&build.manifest)
            .arg("--target")
            .arg(&build.target)
            .arg("--profile")
            .arg(&build.profile)
            .arg("--message-format=json-render-diagnostics");

        if let Some(package) = &build.package {
            command.arg("--package").arg(package);
        }

        if !build.features.is_empty() {
            command.arg("--features").arg(build.features.join(","));
        }

        if build.no_default_features {
            command.arg("--no-default-features");
        }

        if build.locked {
            command.arg("--locked");
        }

        let output = command.output().map_err(|err| {
            wasmtime::Error::msg(format!(
                "Unable to execute Cargo for service '{}': {}. \
                 Ensure Cargo is installed and available in PATH.",
                service_name, err
            ))
        })?;

        if !output.status.success() {
            return Err(wasmtime::Error::msg(format!(
                "Cargo build failed for service '{}'.\n\
                 Manifest: {}\n\
                 Exit status: {}\n\
                 stdout:\n{}\n\
                 stderr:\n{}",
                service_name,
                build.manifest.display(),
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut cdylib_artifacts = Vec::new();
        let mut wasm_artifacts = Vec::new();

        for line in stdout.lines() {
            let Ok(message) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };

            if message["reason"] != "compiler-artifact" {
                continue;
            }

            let is_cdylib = message["target"]["kind"]
                .as_array()
                .is_some_and(|kinds| kinds.iter().any(|kind| kind == "cdylib"));

            let Some(filenames) = message["filenames"].as_array() else {
                continue;
            };

            for filename in filenames {
                let Some(filename) = filename.as_str() else {
                    continue;
                };

                let path = PathBuf::from(filename);

                if path.extension().is_some_and(|ext| ext == "wasm") {
                    wasm_artifacts.push(path.clone());

                    if is_cdylib {
                        cdylib_artifacts.push(path);
                    }
                }
            }
        }

        let candidates = if cdylib_artifacts.is_empty() {
            wasm_artifacts
        } else {
            cdylib_artifacts
        };

        let mut unique_candidates = Vec::new();

        for candidate in candidates {
            if !unique_candidates.contains(&candidate) {
                unique_candidates.push(candidate);
            }
        }

        if unique_candidates.len() != 1 {
            return Err(wasmtime::Error::msg(format!(
                "Build completed for service '{}', but expected exactly one WASM artifact; found {}. \
                 Configure the correct Cargo package if this is a workspace.",
                service_name,
                unique_candidates.len()
            )));
        }

        let generated_artifact = &unique_candidates[0];
        let destination = &service.component;

        if !generated_artifact.is_file() {
            return Err(wasmtime::Error::msg(format!(
                "Cargo reported a WASM artifact that does not exist for service '{}': {}",
                service_name,
                generated_artifact.display()
            )));
        }

        let same_file = generated_artifact
            .canonicalize()
            .ok()
            .zip(destination.canonicalize().ok())
            .is_some_and(|(source, destination)| source == destination);

        if !same_file {
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent).map_err(|err| {
                    wasmtime::Error::msg(format!(
                        "Unable to create artifact destination directory '{}': {}",
                        parent.display(),
                        err
                    ))
                })?;
            }

            fs::copy(generated_artifact, destination).map_err(|err| {
                wasmtime::Error::msg(format!(
                    "Unable to copy generated artifact for service '{}' from '{}' to '{}': {}",
                    service_name,
                    generated_artifact.display(),
                    destination.display(),
                    err
                ))
            })?;
        }

        println!(
            "Build completed: service={} artifact={}",
            service_name,
            destination.display()
        );
    }

    println!("Host-managed artifact build pipeline completed.");
    Ok(())
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

        let metadata = std::fs::metadata(component_path).map_err(|err| {
            wasmtime::Error::msg(format!(
                "Artifact preflight failed for service '{}': \
                 cannot access '{}': {}",
                service_name,
                component_path.display(),
                err
            ))
        })?;

        if !metadata.is_file() {
            return Err(wasmtime::Error::msg(format!(
                "Artifact preflight failed for service '{}': \
                 component path is not a regular file: {}",
                service_name,
                component_path.display()
            )));
        }

        if metadata.len() == 0 {
            return Err(wasmtime::Error::msg(format!(
                "Artifact preflight failed for service '{}': \
                 component artifact is empty: {}",
                service_name,
                component_path.display()
            )));
        }

        File::open(component_path).map_err(|err| {
            wasmtime::Error::msg(format!(
                "Artifact preflight failed for service '{}': \
                 cannot read '{}': {}",
                service_name,
                component_path.display(),
                err
            ))
        })?;

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
