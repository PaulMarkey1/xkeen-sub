use std::error::Error;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

type AppResult<T> = Result<T, Box<dyn Error>>;

const CONFIG_DIR: &str = "/opt/etc/xray/configs";
const ASSET_DIR: &str = "/opt/etc/xray/dat";
const DEFAULT_XRAY_BIN: &str = "/opt/sbin/xray";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XrayConfigInput {
    pub file_name: String,
    pub content: String,
}

impl XrayConfigInput {
    pub fn new(file_name: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            file_name: file_name.into(),
            content: content.into(),
        }
    }
}

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

/// Validates a complete prepared configuration set using temporary copies.
/// The active XKeen/Xray configuration directory is read-only throughout
/// this operation; all generated files are placed under the OS temp directory.
pub fn test_config_set_detailed(
    configs: &[XrayConfigInput],
) -> Result<XrayTestOutput, XrayTestError> {
    let asset_dir = Path::new(ASSET_DIR);
    if !Path::new(CONFIG_DIR).is_dir() {
        return Err(XrayTestError::Setup(format!(
            "Каталог конфигурации Xray не найден: {}",
            CONFIG_DIR
        )));
    }
    if !asset_dir.is_dir() {
        return Err(XrayTestError::Setup(format!(
            "Каталог данных Xray не найден: {}",
            asset_dir.display()
        )));
    }
    if configs.is_empty() {
        return Err(XrayTestError::Setup(
            "Не переданы файлы для проверки конфигурации Xray.".to_owned(),
        ));
    }

    let xray_bin = std::env::var_os("XRAY_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_XRAY_BIN));

    let resources =
        TemporaryResources::create().map_err(|error| XrayTestError::Setup(error.to_string()))?;

    let mut sorted: Vec<&XrayConfigInput> = configs.iter().collect();
    sorted.sort_by(|left, right| left.file_name.cmp(&right.file_name));

    let mut config_paths = Vec::with_capacity(sorted.len());
    for config in sorted {
        let file_name = safe_file_name(&config.file_name).ok_or_else(|| {
            XrayTestError::Setup(format!(
                "Недопустимое имя конфигурационного файла: {}",
                config.file_name
            ))
        })?;
        let path = resources.directory.join(file_name);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(|error| {
                XrayTestError::Setup(format!(
                    "Не удалось создать временную копию {}: {error}",
                    config.file_name
                ))
            })?;
        file.write_all(config.content.as_bytes()).map_err(|error| {
            XrayTestError::Setup(format!(
                "Не удалось записать временную копию {}: {error}",
                config.file_name
            ))
        })?;
        config_paths.push(path);
    }

    let mut command = Command::new(&xray_bin);
    command
        .arg("run")
        .arg("-test")
        .arg("-format=json")
        .env("XRAY_LOCATION_CONFDIR", &resources.directory)
        .env("XRAY_LOCATION_ASSET", asset_dir);

    for path in &config_paths {
        command.arg("-c").arg(path);
    }

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

fn safe_file_name(value: &str) -> Option<&str> {
    let path = Path::new(value);
    if path.components().count() != 1 || path.file_name()?.to_str()? != value {
        return None;
    }
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("json" | "jsonc") => Some(value),
        _ => None,
    }
}

struct TemporaryResources {
    directory: PathBuf,
}

impl TemporaryResources {
    fn create() -> AppResult<Self> {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)?
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "xkeen-sub-xray-test-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&directory).map_err(|error| {
            format!(
                "Не удалось создать временный каталог проверки Xray {}: {error}",
                directory.display()
            )
        })?;
        Ok(Self { directory })
    }
}

impl Drop for TemporaryResources {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

#[cfg(test)]
mod tests {
    use super::{safe_file_name, XrayProcessFailure};

    #[test]
    fn rejects_paths_and_unsupported_extensions() {
        assert_eq!(safe_file_name("05_routing.json"), Some("05_routing.json"));
        assert_eq!(safe_file_name("../05_routing.json"), None);
        assert_eq!(safe_file_name("folder/file.json"), None);
        assert_eq!(safe_file_name("file.yaml"), None);
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
