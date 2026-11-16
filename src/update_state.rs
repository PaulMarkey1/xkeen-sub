use serde_json::Value;
use std::path::PathBuf;

/// A fully prepared subscription group ready for configuration merging.
#[derive(Debug, Clone)]
pub(crate) struct PreparedSubscription {
    pub(crate) id: String,
    pub(crate) outbounds: Vec<Value>,
}

/// Raw subscription response staged for an atomic baseline update.
#[derive(Debug, Clone)]
pub(crate) struct PreparedBaseline {
    pub(crate) id: String,
    pub(crate) original: Vec<u8>,
    pub(crate) path: PathBuf,
}

/// Mutable result accumulated while selected subscriptions are processed.
#[derive(Debug, Default)]
pub(crate) struct RunState {
    pub(crate) prepared: Vec<PreparedSubscription>,
    pub(crate) baseline_updates: Vec<PreparedBaseline>,
    pub(crate) baseline_removals: Vec<(String, PathBuf)>,
    pub(crate) failed: Vec<(String, String)>,
    pub(crate) downloaded: usize,
    pub(crate) processed: usize,
}
