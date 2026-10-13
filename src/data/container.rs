//! Pure container identity values shared by data and engine layers.

/// Stable name for a container (for example, `awman-abc123`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ContainerName(pub String);

impl ContainerName {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}
