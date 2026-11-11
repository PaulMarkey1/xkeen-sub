use std::error::Error;
use std::ffi::OsStr;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::terminal::success;

type AppResult<T> = Result<T, Box<dyn Error>>;

const CONFIG_DIR: &str = "/opt/etc/xray/configs";
const ASSET_DIR: &str = "/opt/etc/xray/dat";
const ACTIVE_OUTBOUND: &str = "04_outbounds.json";
const DEFAULT_XRAY_BIN: &str = "/opt/sbin/xray";

/// Structured Xray process failure. The streams are kept separately because
/// Xray may write diagnostics to either stdout or stderr.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XrayProcessFailure {
    pub exit_code: Option<i32>,
    pub process_status: String,
    pub stdout: String,
    pub stderr: String,
}

impl XrayProcessFailure {
    pub fn combined_diagnostics(&self) -> String {
        match (self.stdout.trim(), self.stderr.trim()) {
            ("", "") => String::new(),
            ("", stderr) => stderr.to_owned(),
            (stdout, "") => stdout.to_owned(),
            (stdout, stderr) => format!("{stdout}\n{stderr}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XrayTestOutput {
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug)]
pub enum XrayTestError {
    Setup(String),
    Process(XrayProcessFailure),
}

impl fmt::Display for XrayTestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Setup(message) => formatter.write_str(message),
            Self::Process(failure) => write!(
                formatter,
                "Проверка Xray завершилась ошибкой (код процесса: {}).",
                failure
                    .exit_code
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| failure.process_status.clone())
            ),
        }
    }
}

impl Error for XrayTestError {}

/// Checks a prepared Outbound document together with the other active Xray
/// configuration files, without loading the currently active Outbound file
/// and without starting the Xray server.
pub fn test_outbounds_detailed(outbounds_json: &str) -> Result<XrayTestOutput, XrayTestError> {
    let config_dir = Path::new(CONFIG_DIR);
    let asset_dir = Path::new(ASSET_DIR);

    if !config_dir.is_dir() {
        return Err(XrayTestError::Setup(format!(
            "Каталог конфигурации Xray не найден: {}",
            config_dir.display()
        )));
    }
    if !asset_dir.is_dir() {
        return Err(XrayTestError::Setup(format!(
            "Каталог данных Xray не найден: {}",
            asset_dir.display()
        )));
    }

    let xray_bin = std::env::var_os("XRAY_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_XRAY_BIN));

    let resources = TemporaryResources::create(config_dir)
        .map_err(|error| XrayTestError::Setup(error.to_string()))?;
    fs::write(&resources.outbound_file, outbounds_json).map_err(|error| {
        XrayTestError::Setup(format!("Не удалось записать временный Outbound: {error}"))
    })?;

    let mut json_configs = Vec::new();
    let mut jsonc_configs = Vec::new();
    let entries = fs::read_dir(config_dir)
        .map_err(|error| XrayTestError::Setup(error.to_string()))?;

    for entry in entries {
        let entry = entry.map_err(|error| XrayTestError::Setup(error.to_string()))?;
        let path = entry.path();

        if path
            .file_name()
            .and_then(OsStr::to_str)
            .map(|name| name.starts_with('.'))
            .unwrap_or(false)
        {
            continue;
        }
        if !path.is_file() || path.file_name() == Some(OsStr::new(ACTIVE_OUTBOUND)) {
            continue;
        }

        match path.extension().and_then(OsStr::to_str) {
            Some("json") => json_configs.push(path),
            Some("jsonc") => jsonc_configs.push(path),
            _ => {}
        }
    }

    json_configs.sort();
    jsonc_configs.sort();
    let mut config_files = json_configs;
    config_files.extend(jsonc_configs);

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
    command.arg("-c").arg(&resources.outbound_file);

    let output = command.output().map_err(|error| {
        XrayTestError::Setup(format!(
            "Не удалось запустить проверку Xray ({}): {error}",
            xray_bin.display()
        ))
    })?;

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    if !output.status.success() {
        return Err(XrayTestError::Process(XrayProcessFailure {
            exit_code: output.status.code(),
            process_status: output.status.to_string(),
            stdout,
            stderr,
        }));
    }

    Ok(XrayTestOutput { stdout, stderr })
}

/// Backwards-compatible wrapper for callers that only need success/failure.
/// The update pipeline should use test_outbounds_detailed to retain the
/// original Xray streams and process exit code for diagnostics.
pub fn test_outbounds(outbounds_json: &str) -> AppResult<()> {
    println!("Проверка конфигурации Xray...");
    println!("Действующий 04_outbounds.json исключён из проверки.");
    println!("Рабочий сервер Xray не запускается.");

    match test_outbounds_detailed(outbounds_json) {
        Ok(output) => {
            io::stdout().write_all(output.stdout.as_bytes())?;
            io::stderr().write_all(output.stderr.as_bytes())?;
            success("Проверка конфигурации Xray завершена успешно.");
            Ok(())
        }
        Err(XrayTestError::Process(failure)) => {
            io::stdout().write_all(failure.stdout.as_bytes())?;
            io::stderr().write_all(failure.stderr.as_bytes())?;
            Err(XrayTestError::Process(failure).into())
        }
        Err(error) => Err(error.into()),
    }
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

        let file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&outbound_file)
        {
            Ok(file) => file,
            Err(error) => {
                let _ = fs::remove_dir(&empty_conf_dir);
                return Err(format!("Не удалось создать временный Outbound: {error}").into());
            }
        };

        drop(file);
        Ok(Self {
            outbound_file,
            empty_conf_dir,
        })
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
    use super::{is_json_config, XrayProcessFailure};
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

    #[test]
    fn structured_failure_preserves_both_streams_and_exit_code() {
        let failure = XrayProcessFailure {
            exit_code: Some(23),
            process_status: "exit status: 23".to_owned(),
            stdout: "stdout diagnostic".to_owned(),
            stderr: "stderr diagnostic".to_owned(),
        };

        assert_eq!(failure.exit_code, Some(23));
        assert_eq!(
            failure.combined_diagnostics(),
            "stdout diagnostic\nstderr diagnostic"
        );
    }
}
