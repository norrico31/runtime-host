use std::{
    fs::{self, File},
    path::PathBuf,
    process::Command,
};

use wasmtime::{Engine, component::Component};

use crate::config::{BuildConfig, BuildDriver, RuntimeConfig, WorkloadConfig};

pub struct PreflightedWorkload {
    pub workload_name: String,
    pub artifact_path: PathBuf,
    pub instances: usize,
    pub memory_limit: u64,
    pub component: Component,
}

pub fn build_configured_artifacts(config: &RuntimeConfig) -> wasmtime::Result<()> {
    println!("Starting host-managed workload build pipeline...");

    for (name, workload) in &config.workloads {
        if !workload.enabled {
            println!("Skipping disabled workload: {name}");
            continue;
        }

        let Some(build) = &workload.build else {
            println!(
                "No build configuration for workload '{}'; using existing artifact.",
                name
            );
            continue;
        };

        println!(
            "Building workload: name={} driver={:?} repository={}",
            name,
            build.driver,
            workload.repository.display()
        );

        if !workload.repository.is_dir() {
            return Err(wasmtime::Error::msg(format!(
                "Build failed for workload '{}': repository directory does not exist: {}",
                name,
                workload.repository.display()
            )));
        }

        if let Some(parent) = workload.artifact.parent() {
            fs::create_dir_all(parent).map_err(|err| {
                wasmtime::Error::msg(format!(
                    "Unable to create artifact directory '{}': {}",
                    parent.display(),
                    err
                ))
            })?;
        }

        match build.driver {
            BuildDriver::Cargo => {
                build_cargo_workload(name, workload, build)?;
            }
            BuildDriver::Command => {
                build_command_workload(name, workload, build)?;
            }
        }

        if !workload.artifact.is_file() {
            return Err(wasmtime::Error::msg(format!(
                "Build completed for workload '{}', but the configured WASM artifact was not produced: {}",
                name,
                workload.artifact.display()
            )));
        }

        if fs::metadata(&workload.artifact)
            .map(|metadata| metadata.len() == 0)
            .unwrap_or(true)
        {
            return Err(wasmtime::Error::msg(format!(
                "Build produced an empty artifact for workload '{}': {}",
                name,
                workload.artifact.display()
            )));
        }

        println!(
            "Build completed: workload={} artifact={}",
            name,
            workload.artifact.display()
        );
    }

    println!("Host-managed workload build pipeline completed.");
    Ok(())
}

fn build_cargo_workload(
    name: &str,
    workload: &WorkloadConfig,
    build: &BuildConfig,
) -> wasmtime::Result<()> {
    let manifest = build.manifest.as_ref().ok_or_else(|| {
        wasmtime::Error::msg(format!(
            "Cargo build for workload '{}' requires a manifest path",
            name
        ))
    })?;

    if !manifest.is_file() {
        return Err(wasmtime::Error::msg(format!(
            "Build failed for workload '{}': Cargo manifest does not exist: {}",
            name,
            manifest.display()
        )));
    }

    let mut command = Command::new("cargo");

    command
        .arg("build")
        .arg("--manifest-path")
        .arg(manifest)
        .arg("--message-format=json-render-diagnostics");

    if let Some(target) = &build.target {
        command.arg("--target").arg(target);
    }

    if let Some(profile) = &build.profile {
        command.arg("--profile").arg(profile);
    }

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
            "Unable to execute Cargo for workload '{}': {}. Ensure Cargo is installed and available in PATH.",
            name, err
        ))
    })?;

    if !output.status.success() {
        return Err(wasmtime::Error::msg(format!(
            "Cargo build failed for workload '{}'.\n\
             Manifest: {}\n\
             Exit status: {}\n\
             stdout:\n{}\n\
             stderr:\n{}",
            name,
            manifest.display(),
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut wasm_artifacts = Vec::<PathBuf>::new();
    let mut cdylib_artifacts = Vec::<PathBuf>::new();

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
            "Cargo build completed for workload '{}', but expected exactly one WASM artifact; found {}. Configure the correct package if this is a workspace.",
            name,
            unique_candidates.len()
        )));
    }

    let generated = &unique_candidates[0];

    if !generated.is_file() {
        return Err(wasmtime::Error::msg(format!(
            "Cargo reported an artifact that does not exist for workload '{}': {}",
            name,
            generated.display()
        )));
    }

    copy_artifact(name, generated, &workload.artifact)
}

fn build_command_workload(
    name: &str,
    workload: &WorkloadConfig,
    build: &BuildConfig,
) -> wasmtime::Result<()> {
    let executable = build.command.as_deref().ok_or_else(|| {
        wasmtime::Error::msg(format!(
            "Command build for workload '{}' requires a command",
            name
        ))
    })?;

    let mut command = Command::new(executable);

    command
        .current_dir(&workload.repository)
        .args(&build.args)
        .env("RUNTIME_WORKLOAD_NAME", name)
        .env("RUNTIME_REPOSITORY", &workload.repository)
        .env("RUNTIME_ARTIFACT", &workload.artifact);

    let output = command.output().map_err(|err| {
        wasmtime::Error::msg(format!(
            "Unable to execute build command '{}' for workload '{}': {}",
            executable, name, err
        ))
    })?;

    if !output.status.success() {
        return Err(wasmtime::Error::msg(format!(
            "Build command failed for workload '{}'.\n\
             Command: {}\n\
             Exit status: {}\n\
             stdout:\n{}\n\
             stderr:\n{}",
            name,
            executable,
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )));
    }

    Ok(())
}

fn copy_artifact(
    workload_name: &str,
    source: &PathBuf,
    destination: &PathBuf,
) -> wasmtime::Result<()> {
    let same_file = source
        .canonicalize()
        .ok()
        .zip(destination.canonicalize().ok())
        .is_some_and(|(source, destination)| source == destination);

    if !same_file {
        fs::copy(source, destination).map_err(|err| {
            wasmtime::Error::msg(format!(
                "Unable to copy artifact for workload '{}' from '{}' to '{}': {}",
                workload_name,
                source.display(),
                destination.display(),
                err
            ))
        })?;
    }

    Ok(())
}

pub fn preflight_components(
    engine: &Engine,
    config: &RuntimeConfig,
) -> wasmtime::Result<Vec<PreflightedWorkload>> {
    println!("Starting workload artifact preflight...");

    let mut preflighted = Vec::new();

    for (name, workload) in &config.workloads {
        if !workload.enabled {
            println!("Skipping disabled workload: {name}");
            continue;
        }

        let path = &workload.artifact;

        println!(
            "Checking WASM component: workload={} path={}",
            name,
            path.display()
        );

        let metadata = std::fs::metadata(path).map_err(|err| {
            wasmtime::Error::msg(format!(
                "Artifact preflight failed for workload '{}': cannot access '{}': {}",
                name,
                path.display(),
                err
            ))
        })?;

        if !metadata.is_file() || metadata.len() == 0 {
            return Err(wasmtime::Error::msg(format!(
                "Artifact preflight failed for workload '{}': artifact is not a non-empty regular file: {}",
                name,
                path.display()
            )));
        }

        File::open(path).map_err(|err| {
            wasmtime::Error::msg(format!(
                "Artifact preflight failed for workload '{}': cannot read '{}': {}",
                name,
                path.display(),
                err
            ))
        })?;

        let component = Component::from_file(engine, path).map_err(|err| {
            wasmtime::Error::msg(format!(
                "Artifact preflight failed for workload '{}': invalid or unsupported WASM component '{}': {}",
                name,
                path.display(),
                err
            ))
        })?;

        println!(
            "Artifact preflight passed: workload={} size={} bytes",
            name,
            metadata.len()
        );

        preflighted.push(PreflightedWorkload {
            workload_name: name.clone(),
            artifact_path: path.clone(),
            instances: workload.instances,
            memory_limit: workload.resources.memory_limit,
            component,
        });
    }

    if preflighted.is_empty() {
        return Err(wasmtime::Error::msg(
            "Artifact preflight failed: no enabled workloads available",
        ));
    }

    println!(
        "Workload artifact preflight completed: {} workload(s) validated",
        preflighted.len()
    );

    Ok(preflighted)
}
