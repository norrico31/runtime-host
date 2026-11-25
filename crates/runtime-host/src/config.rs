use serde::Deserialize;
use std::{
    collections::{BTreeMap, HashSet},
    error::Error,
    fs,
    path::{Path, PathBuf},
};

const MAX_ALLOWED_SERVICES: usize = 5;

fn default_target() -> String {
    "wasm32-wasip2".to_string()
}

fn default_profile() -> String {
    "release".to_string()
}


fn default_gateway_host() -> String {
    "127.0.0.1".to_string()
}

fn default_gateway_port() -> u16 {
    8080
}

fn default_gateway_enabled() -> bool {
    true
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
pub struct RuntimeConfig {
    pub runtime: RuntimeSettings,

    #[serde(default)]
    pub gateway: Option<GatewayConfig>,

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

    #[serde(default)]
    pub build: Option<BuildConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildConfig {
    pub manifest: PathBuf,

    #[serde(default = "default_target")]
    pub target: String,

    #[serde(default = "default_profile")]
    pub profile: String,

    #[serde(default)]
    pub package: Option<String>,

    #[serde(default)]
    pub features: Vec<String>,

    #[serde(default)]
    pub no_default_features: bool,

    #[serde(default)]
    pub locked: bool,
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

            if let Some(build) = &mut service.build {
                if build.manifest.is_relative() {
                    build.manifest = base_dir.join(&build.manifest);
                }
            }

            if service.enabled {
                println!(
                    "Configured service: name={} port={} component={}",
                    service_name,
                    service.port,
                    service.component.display()
                );

                if let Some(build) = &service.build {
                    println!(
                        "Configured build: service={} manifest={} target={} profile={}",
                        service_name,
                        build.manifest.display(),
                        build.target,
                        build.profile
                    );
                }
            }
        }

        Ok(config)
    }

    fn validate(&self) -> Result<(), Box<dyn Error>> {
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

        if self.runtime.max_services == 0 || self.runtime.max_services > MAX_ALLOWED_SERVICES {
            return Err("max_services must be between 1 and 5".into());
        }

        if self.services.is_empty() {
            return Err("At least one service must be configured".into());
        }

        for (service_name, service) in &self.services {
            if service_name.trim().is_empty() {
                return Err("Service name cannot be empty".into());
            }

            if let Some(build) = &service.build {
                if build.manifest.as_os_str().is_empty() {
                    return Err(format!(
                        "Build manifest cannot be empty for service '{}'",
                        service_name
                    )
                    .into());
                }

                if build.target.trim().is_empty() {
                    return Err(format!(
                        "Build target cannot be empty for service '{}'",
                        service_name
                    )
                    .into());
                }

                if build.profile.trim().is_empty() {
                    return Err(format!(
                        "Build profile cannot be empty for service '{}'",
                        service_name
                    )
                    .into());
                }
            }
        }

        let enabled: Vec<_> = self
            .services
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

        if let Some(gateway) = &self.gateway {
            if gateway.enabled && !ports.insert(gateway.port) {
                return Err(
                    format!("Gateway port conflicts with a service port: {}", gateway.port)
                        .into()
                );
            }
        }

        for service in enabled {
            if service.port == 0 {
                return Err("Service port cannot be zero".into());
            }

            if !ports.insert(service.port) {
                return Err(format!("Duplicate service port: {}", service.port).into());
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn valid_config() -> &'static str {
        r#"
[runtime]
max_services = 2

[services.users]
component = "users.wasm"
port = 8081
enabled = true

[services.orders]
component = "orders.wasm"
port = 8082
enabled = true
"#
    }

    fn parse_config(contents: &str) -> Result<RuntimeConfig, Box<dyn Error>> {
        let config: RuntimeConfig = toml::from_str(contents)?;
        config.validate()?;
        Ok(config)
    }

    fn create_test_dir() -> PathBuf {
        let id = TEST_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);

        let path = std::env::temp_dir().join(format!(
            "runtime-host-config-test-{}-{}",
            std::process::id(),
            id
        ));

        fs::create_dir_all(&path).expect("failed to create test directory");
        path
    }

    #[test]
    fn accepts_valid_configuration() {
        assert!(parse_config(valid_config()).is_ok());
    }

    #[test]
    fn accepts_five_enabled_services() {
        let contents = r#"
[runtime]
max_services = 5

[services.one]
component = "one.wasm"
port = 8001
enabled = true

[services.two]
component = "two.wasm"
port = 8002
enabled = true

[services.three]
component = "three.wasm"
port = 8003
enabled = true

[services.four]
component = "four.wasm"
port = 8004
enabled = true

[services.five]
component = "five.wasm"
port = 8005
enabled = true
"#;

        assert!(parse_config(contents).is_ok());
    }

    #[test]
    fn accepts_build_configuration() {
        let contents = r#"
[runtime]
max_services = 1

[services.users]
component = "users.wasm"
port = 8081
enabled = true

[services.users.build]
manifest = "Cargo.toml"
target = "wasm32-wasip2"
profile = "release"
"#;

        let config = parse_config(contents).expect("build config should be valid");
        let build = config.services["users"].build.as_ref().unwrap();

        assert_eq!(build.target, "wasm32-wasip2");
        assert_eq!(build.profile, "release");
    }

    #[test]
    fn ignores_commented_out_service_and_port_entries() {
        let contents = r#"
[runtime]
max_services = 1

[services.users]
component = "users.wasm"
port = 8081
# port = 000

# [services.orders]
# component = "orders.wasm"
# port = 8082
# enabled = true

enabled = true
"#;

        let config = parse_config(contents).expect("configuration should be valid");

        assert_eq!(config.services.len(), 1);
        assert_eq!(config.services["users"].port, 8081);
        assert!(config.services["users"].enabled);
    }

    #[test]
    fn rejects_zero_max_services() {
        let contents = valid_config().replace("max_services = 2", "max_services = 0");
        assert!(parse_config(&contents).is_err());
    }

    #[test]
    fn rejects_max_services_above_limit() {
        let contents = valid_config().replace("max_services = 2", "max_services = 6");
        assert!(parse_config(&contents).is_err());
    }

    #[test]
    fn rejects_enabled_services_above_limit() {
        let contents = valid_config().replace("max_services = 2", "max_services = 1");
        assert!(parse_config(&contents).is_err());
    }

    #[test]
    fn rejects_configuration_without_enabled_services() {
        let contents = valid_config().replace("enabled = true", "enabled = false");
        assert!(parse_config(&contents).is_err());
    }

    #[test]
    fn rejects_zero_service_port() {
        let contents = valid_config().replace("port = 8081", "port = 0");
        assert!(parse_config(&contents).is_err());
    }

    #[test]
    fn rejects_duplicate_enabled_service_ports() {
        let contents = valid_config().replace("port = 8082", "port = 8081");
        assert!(parse_config(&contents).is_err());
    }

    #[test]
    fn ignores_duplicate_ports_on_disabled_services() {
        let contents = r#"
[runtime]
max_services = 1

[services.users]
component = "users.wasm"
port = 8081
enabled = true

[services.orders]
component = "orders.wasm"
port = 8081
enabled = false
"#;

        assert!(parse_config(contents).is_ok());
    }

    #[test]
    fn rejects_empty_service_name() {
        let contents = r#"
[runtime]
max_services = 1

[services]
"" = { component = "users.wasm", port = 8081, enabled = true }
"#;

        assert!(parse_config(contents).is_err());
    }

    #[test]
    fn rejects_unknown_root_field() {
        let contents = format!("unknown = true\n{}", valid_config());
        assert!(parse_config(&contents).is_err());
    }

    #[test]
    fn rejects_unknown_runtime_field() {
        let contents =
            valid_config().replace("max_services = 2", "max_services = 2\nunknown = true");
        assert!(parse_config(&contents).is_err());
    }

    #[test]
    fn rejects_unknown_service_field() {
        let contents = valid_config().replace("enabled = true", "enabled = true\nunknown = true");
        assert!(parse_config(&contents).is_err());
    }

    #[test]
    fn rejects_missing_required_field() {
        let contents = valid_config().replace("port = 8081\n", "");
        assert!(parse_config(&contents).is_err());
    }

    #[test]
    fn rejects_invalid_toml() {
        let contents = "[runtime\nmax_services = 2";
        assert!(parse_config(contents).is_err());
    }

    #[test]
    fn resolves_relative_component_paths_from_config_directory() {
        let test_dir = create_test_dir();
        let config_path = test_dir.join("wasm-services.toml");

        fs::write(&config_path, valid_config()).expect("failed to write test configuration");

        let config = RuntimeConfig::load(&config_path).expect("configuration should load");

        assert_eq!(
            config.services["users"].component,
            test_dir.join("users.wasm")
        );

        assert_eq!(
            config.services["orders"].component,
            test_dir.join("orders.wasm")
        );

        fs::remove_dir_all(&test_dir).expect("failed to remove test directory");
    }

    #[test]
    fn resolves_build_manifest_path_from_config_directory() {
        let test_dir = create_test_dir();
        let config_path = test_dir.join("wasm-services.toml");

        let contents = r#"
[runtime]
max_services = 1

[services.users]
component = "users.wasm"
port = 8081
enabled = true

[services.users.build]
manifest = "../guest/Cargo.toml"
"#;

        fs::write(&config_path, contents).expect("failed to write test configuration");

        let config = RuntimeConfig::load(&config_path).expect("configuration should load");

        assert_eq!(
            config.services["users"].build.as_ref().unwrap().manifest,
            test_dir.join("../guest/Cargo.toml")
        );

        fs::remove_dir_all(&test_dir).expect("failed to remove test directory");
    }
}
