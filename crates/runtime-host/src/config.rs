
use serde::Deserialize;
use std::{
    collections::{BTreeMap, HashSet},
    error::Error,
    fs,
    path::{Path, PathBuf},
};

const MAX_ALLOWED_SERVICES: usize = 5;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeConfig {
    pub runtime: RuntimeSettings,
    pub services: BTreeMap<String, ServiceConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSettings {
    pub max_services: usize,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceConfig {
    pub component: PathBuf,
    pub port: u16,
    pub enabled: bool,
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

        config.validate()?;

        let base_dir = resolved_config_path
            .parent()
            .unwrap_or_else(|| Path::new("."));

        for (service_name, service) in &mut config.services {
            if service.component.is_relative() {
                service.component = base_dir.join(&service.component);
            }

            if service.enabled {
                println!(
                    "Configured service: name={} port={} component={}",
                    service_name,
                    service.port,
                    service.component.display()
                );
            }
        }

        Ok(config)
    }

    fn validate(&self) -> Result<(), Box<dyn Error>> {
        if self.runtime.max_services == 0
            || self.runtime.max_services > MAX_ALLOWED_SERVICES
        {
            return Err("max_services must be between 1 and 5".into());
        }

        let enabled: Vec<_> = self.services
            .values()
            .filter(|service| service.enabled)
            .collect();

        if enabled.is_empty() {
            return Err("At least one service must be enabled".into());
        }

        if enabled.len() > self.runtime.max_services {
            return Err("Enabled services exceed max_services".into());
        }

        let mut ports = HashSet::new();

        for service in &enabled {
            if service.port == 0 {
                return Err("Service port cannot be zero".into());
            }

            if !ports.insert(service.port) {
                return Err(format!(
                    "Duplicate service port: {}",
                    service.port
                ).into());
            }
        }

        Ok(())
    }
}

