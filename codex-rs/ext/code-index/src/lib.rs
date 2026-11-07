//! Opt-in code-index retrieval for turn input context.

mod extension;

pub use extension::CodeIndexExtensionConfig;
pub use extension::DEFAULT_CODE_INDEX_MAX_RESULTS;
pub use extension::DEFAULT_CODE_INDEX_TOKEN_BUDGET;
pub use extension::install;
pub use extension::install_with_embedding_provider;
