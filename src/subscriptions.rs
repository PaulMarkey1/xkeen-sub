use serde::{Deserialize, Serialize};
use std::error::Error;
use std::fs;
use std::path::Path;

const SUBSCRIPTIONS_FILE: &str = "/opt/etc/xkeen-sub/sub.json";

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct SubscriptionStore {
    #[serde(default)]
    pub subscriptions: Vec<Subscription>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Subscription {
    pub id: String,
    pub name: Option<String>,
    pub url: String,
    pub prefix: String,
}

pub fn load_subscriptions() -> Result<SubscriptionStore, Box<dyn Error>> {
    let path = Path::new(SUBSCRIPTIONS_FILE);
    if !path.exists() {
        return Ok(SubscriptionStore::default());
    }
    let content = fs::read_to_string(path)?;
    let store = serde_json::from_str(&content)?;
    Ok(store)
}

pub fn save_subscriptions(store: &SubscriptionStore) -> Result<(), Box<dyn Error>> {
    let path = Path::new(SUBSCRIPTIONS_FILE);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(store)?;
    fs::write(path, format!("{content}\n"))?;
    Ok(())
}
