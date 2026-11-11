use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::error::Error;
use std::fs;
use std::path::Path;

use crate::terminal::{info, success, title};

const CONFIG_FILE: &str = "/opt/etc/xkeen-sub/config.json";

type AppResult<T> = Result<T, Box<dyn Error>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ValidationMode {
    On,
    Off,
    Alert,
}

impl ValidationMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::On => "on",
            Self::Off => "off",
            Self::Alert => "alert",
        }
    }
}

impl Default for ValidationMode {
    fn default() -> Self {
        Self::Alert
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub validation: ValidationMode,
    // Preserve settings added by future versions when this version saves config.json.
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            validation: ValidationMode::default(),
            extra: BTreeMap::new(),
        }
    }
}

pub fn load() -> AppResult<AppConfig> {
    let path = Path::new(CONFIG_FILE);
    if !path.exists() {
        return Ok(AppConfig::default());
    }

    let content = fs::read_to_string(path)?;
    let config = serde_json::from_str(&content)
        .map_err(|error| format!("Не удалось прочитать config.json: {error}"))?;
    Ok(config)
}

pub fn show_validation() -> AppResult<()> {
    let config = load()?;
    title("НАСТРОЙКА ВАЛИДАЦИИ");
    info(&format!("Текущий режим: {}", config.validation.as_str()));
    Ok(())
}

pub fn set_validation(mode: ValidationMode) -> AppResult<()> {
    let mut config = load()?;
    config.validation = mode;
    save(&config)?;
    success(&format!("Режим валидации установлен: {}.", mode.as_str()));
    Ok(())
}

fn save(config: &AppConfig) -> AppResult<()> {
    let path = Path::new(CONFIG_FILE);
    let parent = path.parent().ok_or("Не удалось определить каталог config.json.")?;
    fs::create_dir_all(parent)?;

    let content = serde_json::to_string_pretty(config)?;
    let temporary = parent.join(format!(".config.json.tmp.{}", std::process::id()));

    let write_result = (|| -> AppResult<()> {
        fs::write(&temporary, format!("{content}\n"))?;
        fs::rename(&temporary, path)?;
        Ok(())
    })();

    if write_result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    write_result
}

#[cfg(test)]
mod tests {
    use super::{AppConfig, ValidationMode};

    #[test]
    fn missing_validation_defaults_to_alert() {
        let config: AppConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(config.validation, ValidationMode::Alert);
    }

    #[test]
    fn validation_modes_use_lowercase_config_values() {
        for (value, expected) in [
            ("on", ValidationMode::On),
            ("off", ValidationMode::Off),
            ("alert", ValidationMode::Alert),
        ] {
            let json = format!(r#"{{"validation":"{value}"}}"#);
            let config: AppConfig = serde_json::from_str(&json).unwrap();
            assert_eq!(config.validation, expected);
        }
    }

    #[test]
    fn unknown_settings_are_preserved() {
        let config: AppConfig =
            serde_json::from_str(r#"{"validation":"on","future_setting":true}"#).unwrap();
        let saved = serde_json::to_value(config).unwrap();
        assert_eq!(saved["future_setting"], true);
        assert_eq!(saved["validation"], "on");
    }
}
