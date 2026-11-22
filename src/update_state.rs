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

/// A subscription whose only outbound changes are Xray shortId rotations.
/// It is promoted to a real update only when another subscription has meaningful changes.
#[derive(Debug, Clone)]
pub(crate) struct DeferredShortIdUpdate {
    pub(crate) subscription: PreparedSubscription,
    pub(crate) baseline: PreparedBaseline,
}

/// Tag-level differences between the saved baseline and the downloaded subscription.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SubscriptionChanges {
    pub(crate) id: String,
    pub(crate) added: Vec<String>,
    pub(crate) removed: Vec<String>,
    pub(crate) changed: Vec<String>,
}

/// Mutable result accumulated while selected subscriptions are processed.
#[derive(Debug, Default)]
pub(crate) struct RunState {
    pub(crate) subscription_changes: Vec<SubscriptionChanges>,
    pub(crate) prepared: Vec<PreparedSubscription>,
    pub(crate) baseline_updates: Vec<PreparedBaseline>,
    pub(crate) deferred_short_id_updates: Vec<DeferredShortIdUpdate>,
    pub(crate) baseline_removals: Vec<(String, PathBuf)>,
    /// Complete source Tag sets from successfully parsed subscriptions.
    /// Partial/empty parses are deliberately not recorded for blacklist cleanup.
    pub(crate) observed_tags: Vec<(String, Vec<String>)>,
    pub(crate) failed: Vec<(String, String)>,
    pub(crate) downloaded: usize,
    pub(crate) processed: usize,
}
