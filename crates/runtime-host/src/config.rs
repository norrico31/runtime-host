
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    error::Error,
    fs,
    path::{Path, PathBuf},
};

fn default_gateway_host() -> String {
    "127.0.0.1".to_string()
}

fn default_gateway_port() -> u16 {
    8080
}

fn default_gateway_enabled() -> bool {
    true
}

fn default_max_instances() -> usize {
    100
}

fn default_instances() -> usize {
    1
}

fn default_memory_limit() -> u64 {
    256 * 1024 * 1024
}

fn default_enabled() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Architecture {
    Monolithic,
    Microservices,
    Hybrid,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkloadKind {
    Monolith,
    Service,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
pub enum BuildDriver {
    Cargo,
    Command,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeploymentConfig {
    pub architecture: Architecture,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GatewayConfig {
    #[serde(default = "default_gateway_host")]
    pub host: String,

    #[serde(default = "default_gateway_port")]
    pub port: u16,

    #[serde(default = "default_gateway_enabled")]
    pub enabled: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSettings {
    #[serde(default = "default_max_instances")]
    pub max_instances: usize,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeConfig {
    pub deployment: DeploymentConfig,
    pub runtime: RuntimeSettings,

    #[serde(default)]
    pub gateway: Option<GatewayConfig>,

    pub workloads: BTreeMap<String, WorkloadConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkloadConfig {
    pub kind: WorkloadKind,
    pub repository: PathBuf,
    pub artifact: PathBuf,

    #[serde(default = "default_enabled")]
    pub enabled: bool,

    #[serde(default = "default_instances")]
    pub instances: usize,

    #[serde(default)]
    pub build: Option<BuildConfig>,

    #[serde(default)]
    pub resources: ResourceConfig,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildConfig {
    pub driver: BuildDriver,

    // Cargo-specific options.
    #[serde(default)]
    pub manifest: Option<PathBuf>,

    #[serde(default)]
    pub target: Option<String>,

    #[serde(default)]
    pub profile: Option<String>,

    #[serde(default)]
    pub package: Option<String>,

    #[serde(default)]
    pub features: Vec<String>,

    #[serde(default)]
    pub no_default_features: bool,

    #[serde(default)]
    pub locked: bool,

    // Generic command-driver options.
    #[serde(default)]
    pub command: Option<String>,

    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceConfig {
    #[serde(default = "default_memory_limit")]
    pub memory_limit: u64,
}

impl Default for ResourceConfig {
    fn default() -> Self {
        Self {
            memory_limit: default_memory_limit(),
        }
    }
}

impl RuntimeConfig {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, Box<dyn Error>> {
        let config_path = path.as_ref();

        let resolved_config_path = if config_path.is_absolute() {
            config_path.to_path_buf()
        } else {
            std::env::current_dir()?.join(config_path)
        };

        println!(
            "Loading runtime configuration: {}",
            resolved_config_path.display()
        );

        let contents = fs::read_to_string(&resolved_config_path)?;
        let mut config: Self = toml::from_str(&contents)?;

        let base_dir = resolved_config_path
            .parent()
            .unwrap_or_else(|| Path::new("."));

        // Resolve all repository, artifact, and manifest paths relative
        // to the configuration file or its repository.
        for (name, workload) in &mut config.workloads {
            if workload.repository.is_relative() {
                workload.repository = base_dir.join(&workload.repository);
            }

            if workload.artifact.is_relative() {
                workload.artifact = workload.repository.join(&workload.artifact);
            }

            if let Some(build) = &mut workload.build {
                if let Some(manifest) = &mut build.manifest {
                    if manifest.is_relative() {
                        *manifest = workload.repository.join(&*manifest);
                    }
                }
            }

            if workload.enabled {
                println!(
                    "Configured workload: name={} kind={:?} repository={} artifact={} instances={}",
                    name,
                    workload.kind,
                    workload.repository.display(),
                    workload.artifact.display(),
                    workload.instances
                );
            }
        }

        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), Box<dyn Error>> {
        if self.runtime.max_instances == 0 {
            return Err("runtime.max_instances must be greater than zero".into());
        }

        if self.workloads.is_empty() {
            return Err("At least one workload must be configured".into());
        }

        if let Some(gateway) = &self.gateway {
            if gateway.enabled {
                if gateway.host.trim().is_empty() {
                    return Err("Gateway host cannot be empty".into());
                }

                if gateway.port == 0 {
                    return Err("Gateway port cannot be zero".into());
                }
            }
        }

        let mut enabled_count = 0usize;
        let mut monolith_count = 0usize;
        let mut service_count = 0usize;

        for (name, workload) in &self.workloads {
            if name.trim().is_empty() {
                return Err("Workload name cannot be empty".into());
            }

            if !workload.enabled {
                continue;
            }

            enabled_count += 1;

            if workload.repository.as_os_str().is_empty() {
                return Err(format!("Repository cannot be empty for workload '{name}'").into());
            }

            if workload.artifact.as_os_str().is_empty() {
                return Err(format!("Artifact path cannot be empty for workload '{name}'").into());
            }

            if workload.instances == 0 {
                return Err(format!("Instances must be greater than zero for workload '{name}'").into());
            }

            if workload.resources.memory_limit == 0 {
                return Err(format!("Memory limit must be greater than zero for workload '{name}'").into());
            }

            match workload.kind {
                WorkloadKind::Monolith => monolith_count += 1,
                WorkloadKind::Service => service_count += 1,
            }

            if let Some(build) = &workload.build {
                match build.driver {
                    BuildDriver::Cargo => {
                        if build.manifest.is_none() {
                            return Err(format!(
                                "Cargo build requires 'manifest' for workload '{name}'"
                            ).into());
                        }

                        if build.command.is_some() {
                            return Err(format!(
                                "Cargo workload '{name}' cannot define 'command'"
                            ).into());
                        }
                    }
                    BuildDriver::Command => {
                        if build.command.as_deref().unwrap_or("").trim().is_empty() {
                            return Err(format!(
                                "Command build requires a non-empty 'command' for workload '{name}'"
                            ).into());
                        }
                    }
                }
            }
        }

        if enabled_count == 0 {
            return Err("At least one workload must be enabled".into());
        }

        let total_instances: usize = self
            .workloads
            .values()
            .filter(|workload| workload.enabled)
            .map(|workload| workload.instances)
            .sum();

        if total_instances > self.runtime.max_instances {
            return Err(format!(
                "Configured instance count ({total_instances}) exceeds runtime.max_instances ({})",
                self.runtime.max_instances
            ).into());
        }

        match self.deployment.architecture {
            Architecture::Monolithic => {
                if monolith_count == 0 || service_count > 0 {
                    return Err(
                        "Monolithic architecture requires monolith workloads only".into()
                    );
                }
            }
            Architecture::Microservices => {
                if service_count == 0 || monolith_count > 0 {
                    return Err(
                        "Microservices architecture requires service workloads only".into()
                    );
                }
            }
            Architecture::Hybrid => {
                if monolith_count == 0 || service_count == 0 {
                    return Err(
                        "Hybrid architecture requires at least one monolith and one service workload".into()
                    );
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_config() -> &'static str {
        r#"
[deployment]
architecture = "microservices"

[runtime]
max_instances = 100

[workloads.users]
kind = "service"
repository = "../users"
artifact = "build/users.wasm"
enabled = true
instances = 10

[workloads.users.build]
driver = "cargo"
manifest = "Cargo.toml"
target = "wasm32-wasip2"
profile = "release"

[workloads.users.resources]
memory_limit = 268435456
"#
    }

    #[test]
    fn accepts_valid_microservices_configuration() {
        let config: RuntimeConfig = toml::from_str(valid_config()).unwrap();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn rejects_zero_max_instances() {
        let contents = valid_config().replace("max_instances = 100", "max_instances = 0");
        let config: RuntimeConfig = toml::from_str(&contents).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_instance_count_above_limit() {
        let contents = valid_config()
            .replace("max_instances = 100", "max_instances = 5");
        let config: RuntimeConfig = toml::from_str(&contents).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_missing_cargo_manifest() {
        let contents = valid_config().replace("manifest = \"Cargo.toml\"\n", "");
        let config: RuntimeConfig = toml::from_str(&contents).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn accepts_hybrid_architecture() {
        let contents = r#"
[deployment]
architecture = "hybrid"

[runtime]
max_instances = 10

[workloads.main]
kind = "monolith"
repository = "../main"
artifact = "main.wasm"
instances = 1

[workloads.api]
kind = "service"
repository = "../api"
artifact = "api.wasm"
instances = 2
"#;

        let config: RuntimeConfig = toml::from_str(contents).unwrap();
        assert!(config.validate().is_ok());
    }
}

