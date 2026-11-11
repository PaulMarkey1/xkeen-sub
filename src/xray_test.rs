use std::error::Error;
use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::terminal::{info, success};

type AppResult<T> = Result<T, Box<dyn Error>>;

const CONFIG_DIR: &str = "/opt/etc/xray/configs";
const ASSET_DIR: &str = "/opt/etc/xray/dat";
const ACTIVE_OUTBOUND: &str = "04_outbounds.json";
const DEFAULT_XRAY_BIN: &str = "/opt/sbin/xray";

/// Checks a prepared Outbound document together with the other active Xray
/// configuration files, without loading the currently active Outbound file
/// and without starting the Xray server.
pub fn test_outbounds(outbounds_json: &str) -> AppResult<()> {
    let config_dir = Path::new(CONFIG_DIR);
    let asset_dir = Path::new(ASSET_DIR);

    if !config_dir.is_dir() {
        return Err(format!("Каталог конфигурации Xray не найден: {}", config_dir.display()).into());
    }
    if !asset_dir.is_dir() {
        return Err(format!("Каталог данных Xray не найден: {}", asset_dir.display()).into());
    }

    let xray_bin = std::env::var_os("XRAY_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_XRAY_BIN));

    let resources = TemporaryResources::create(config_dir)?;
    fs::write(&resources.outbound_file, outbounds_json)
        .map_err(|error| format!("Не удалось записать временный Outbound: {error}"))?;

    let mut config_files = Vec::new();
    for entry in fs::read_dir(config_dir)? {
        let entry = entry?;
        let path = entry.path();

        // Match the shell glob used in the validated router-side test:
        // hidden files are not part of the active configuration set.
        if path.file_name().and_then(OsStr::to_str)
            .map(|name| name.starts_with('.'))
            .unwrap_or(false)
        {
            continue;
        }
        if !entry.file_type()?.is_file() || !is_json_config(&path) {
            continue;
        }
        if path.file_name() == Some(OsStr::new(ACTIVE_OUTBOUND)) {
            continue;
        }

        config_files.push(path);
    }
    config_files.sort();

    info("Проверка конфигурации Xray...");
    info("Действующий 04_outbounds.json исключён из проверки.");
    info("Рабочий сервер Xray не запускается.");

    let mut command = Command::new(&xray_bin);
    command
        .arg("run")
        .arg("-test")
        .arg("-format=json")
        .env("XRAY_LOCATION_CONFDIR", &resources.empty_conf_dir)
        .env("XRAY_LOCATION_ASSET", asset_dir);

    for path in &config_files {
        command.arg("-c").arg(path);
    }
    // The candidate Outbound is always supplied last and is never written
    // over the live 04_outbounds.json file.
    command.arg("-c").arg(&resources.outbound_file);

    let output = command.output().map_err(|error| {
        format!("Не удалось запустить проверку Xray ({}): {error}", xray_bin.display())
    })?;

    io::stdout().write_all(&output.stdout)?;
    io::stderr().write_all(&output.stderr)?;

    if !output.status.success() {
        return Err(format!(
            "Проверка конфигурации Xray завершилась ошибкой (код: {}). Рабочая конфигурация не изменена.",
            output.status
        ).into());
    }

    success("Проверка конфигурации Xray завершена успешно.");
    Ok(())
}

fn is_json_config(path: &Path) -> bool {
    matches!(
        path.extension().and_then(OsStr::to_str),
        Some("json" | "jsonc")
    )
}

struct TemporaryResources {
    outbound_file: PathBuf,
    empty_conf_dir: PathBuf,
}

impl TemporaryResources {
    fn create(config_dir: &Path) -> AppResult<Self> {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)?
            .as_nanos();
        let suffix = format!("{}-{nonce}", std::process::id());
        let outbound_file = config_dir.join(format!(".xkeen-sub-test-{suffix}.json"));
        let empty_conf_dir = std::env::temp_dir().join(format!("xkeen-sub-confdir-{suffix}"));

        fs::create_dir(&empty_conf_dir).map_err(|error| {
            format!("Не удалось создать временный каталог Xray: {error}")
        })?;

        let resources = Self {
            outbound_file,
            empty_conf_dir,
        };

        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&resources.outbound_file);

        if let Err(error) = file {
            return Err(format!("Не удалось создать временный Outbound: {error}").into());
        }

        Ok(resources)
    }
}

impl Drop for TemporaryResources {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.outbound_file);
        let _ = fs::remove_dir(&self.empty_conf_dir);
    }
}

#[cfg(test)]
mod tests {
    use super::is_json_config;
    use std::path::Path;

    #[test]
    fn recognizes_json_and_jsonc_configuration_files() {
        assert!(is_json_config(Path::new("/etc/xray/inbounds.json")));
        assert!(is_json_config(Path::new("/etc/xray/custom.jsonc")));
    }

    #[test]
    fn ignores_other_file_extensions() {
        assert!(!is_json_config(Path::new("/etc/xray/README.md")));
        assert!(!is_json_config(Path::new("/etc/xray/config.yaml")));
    }
}
