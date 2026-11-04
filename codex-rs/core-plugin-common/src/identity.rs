//! Stable local identity shared by plugin catalogs and runtime attribution.

/// Stable key used to merge discoveries of the same logical plugin.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum PluginIdentity {
    /// Local config key (normally `name@marketplace`), or a legacy selected-root identity.
    Local { plugin_id: String },
}

impl PluginIdentity {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Local { plugin_id } => plugin_id,
        }
    }
}
