use std::{collections::BTreeMap, net::SocketAddr};

#[derive(Debug, Clone)]
pub struct ServiceEndpoint {
    pub name: String,
    pub address: SocketAddr,
}

#[derive(Debug, Clone)]
pub struct ServiceRegistry {
    services: BTreeMap<String, ServiceEndpoint>,
}

impl ServiceRegistry {
    pub fn new() -> Self {
        Self {
            services: BTreeMap::new(),
        }
    }

    pub fn register(&mut self, name: impl Into<String>, address: SocketAddr) -> Result<(), String> {
        let name = name.into();

        if name.trim().is_empty() {
            return Err("Service name cannot be empty".into());
        }

        if self.services.contains_key(&name) {
            return Err(format!("Service '{}' is already registered", name));
        }

        let endpoint = ServiceEndpoint {
            name: name.clone(),
            address,
        };

        self.services.insert(name, endpoint);

        Ok(())
    }

    pub fn resolve(&self, name: &str) -> Option<&ServiceEndpoint> {
        self.services.get(name)
    }

    pub fn contains(&self, name: &str) -> bool {
        self.services.contains_key(name)
    }

    pub fn len(&self) -> usize {
        self.services.len()
    }

    pub fn is_empty(&self) -> bool {
        self.services.is_empty()
    }

    pub fn service_names(&self) -> impl Iterator<Item = &String> {
        self.services.keys()
    }
}
