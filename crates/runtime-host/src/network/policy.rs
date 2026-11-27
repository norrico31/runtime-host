use std::collections::BTreeSet;

#[derive(Debug, Clone)]
pub struct NetworkPolicy {
    allowed: BTreeSet<(String, String)>,
}

impl NetworkPolicy {
    pub fn new() -> Self {
        Self {
            allowed: BTreeSet::new(),
        }
    }

    pub fn allow(&mut self, source: impl Into<String>, destination: impl Into<String>) {
        self.allowed.insert((source.into(), destination.into()));
    }

    pub fn is_allowed(&self, source: &str, destination: &str) -> bool {
        self.allowed
            .contains(&(source.to_string(), destination.to_string()))
    }
}
