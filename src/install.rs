use serde_json::Value;
use std::error::Error;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use crate::terminal::{info, success};

type AppResult<T> = Result<T, Box<dyn Error>>;

const XRAY_CONFIG_DIR: &str = "/opt/etc/xray/configs";
const OBSERVATORY_FILE: &str = "observatory.json";
const OBSERVATORY_PLACEHOLDER: &[u8] = b"{}\n";

/// Called by install.sh after the shell installer has verified XKeen, Xray,
/// and the required Xray directories. Reads every JSON/JSONC file in the
/// active Xray config directory, including user-created files.
pub fn ensure_observatory() -> AppResult<()> {
    let config_dir = Path::new(XRAY_CONFIG_DIR);
    if !config_dir.is_dir() {
        return Err(format!(
            "Каталог конфигурации Xray не найден: {}",
            config_dir.display()
        )
        .into());
    }

    let mut files = Vec::new();
    for entry in fs::read_dir(config_dir)? {
        let entry = entry?;
        let path = entry.path();

        if !path.is_file() || !is_json_config(&path) {
            continue;
        }

        files.push(path);
    }
    files.sort();

    let mut found = Vec::new();

    for path in files {
        let content = fs::read_to_string(&path).map_err(|error| {
            format!("Не удалось прочитать конфигурационный файл {}: {error}", path.display())
        })?;

        let document: Value = json5::from_str(&content).map_err(|error| {
            format!("Не удалось разобрать конфигурационный файл {}: {error}", path.display())
        })?;

        let keys = find_observatory_objects(&document).map_err(|error| {
            format!("Некорректный объект в файле {}: {error}", path.display())
        })?;

        for key in keys {
            found.push(format!("{} ({key})", path.display()));
        }
    }

    if !found.is_empty() {
        for item in &found {
            info(&format!("Найден объект Observatory: {item}"));
        }
        success("Проверка Observatory завершена.");
        return Ok(());
    }

    create_observatory_placeholder(config_dir)
}

fn is_json_config(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| {
            extension.eq_ignore_ascii_case("json")
                || extension.eq_ignore_ascii_case("jsonc")
        })
        .unwrap_or(false)
}

fn find_observatory_objects(document: &Value) -> Result<Vec<&'static str>, String> {
    let root = match document.as_object() {
        Some(root) => root,
        None => return Ok(Vec::new()),
    };

    let mut found = Vec::new();

    for key in ["observatory", "burstobservatory"] {
        let Some(value) = root.get(key) else {
            continue;
        };

        let object = value
            .as_object()
            .ok_or_else(|| format!("объект верхнего уровня `{key}` должен быть JSON-объектом"))?;

        if !object.contains_key("subjectSelector") {
            return Err(format!(
                "в объекте верхнего уровня `{key}` отсутствует обязательное поле `subjectSelector`"
            ));
        }

        found.push(key);
    }

    Ok(found)
}

fn create_observatory_placeholder(config_dir: &Path) -> AppResult<()> {
    let path: PathBuf = config_dir.join(OBSERVATORY_FILE);

    match fs::read_to_string(&path) {
        Ok(existing) => {
            let parsed: Value = json5::from_str(&existing).map_err(|error| {
                format!(
                    "Объект Observatory не найден. Файл {} уже существует, но не является корректным JSON: {error}. Файл не изменён.",
                    path.display()
                )
            })?;

            if parsed.as_object().map(|object| object.is_empty()).unwrap_or(false) {
                info(&format!(
                    "Файл {} уже существует как пустой шаблон; он оставлен без изменений.",
                    path.display()
                ));
                return Ok(());
            }

            return Err(format!(
                "Объекты `observatory` и `burstobservatory` с полем `subjectSelector` не найдены. Файл {} уже существует и не будет перезаписан.",
                path.display()
            )
            .into());
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!(
                "Не удалось проверить файл {}: {error}",
                path.display()
            )
            .into());
        }
    }

    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(&path)
        .map_err(|error| {
            format!("Не удалось создать файл {}: {error}", path.display())
        })?;

    if let Err(error) = file.write_all(OBSERVATORY_PLACEHOLDER) {
        let _ = fs::remove_file(&path);
        return Err(format!("Не удалось записать файл {}: {error}", path.display()).into());
    }

    success(&format!(
        "Объекты Observatory не найдены. Создан файл {} с пустым JSON-шаблоном.",
        path.display()
    ));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{find_observatory_objects, is_json_config};
    use serde_json::Value;
    use std::path::Path;

    #[test]
    fn finds_observatory_with_subject_selector() {
        let value: Value = serde_json::from_str(
            r#"{"observatory":{"subjectSelector":["proxy"]}}"#,
        )
        .unwrap();

        assert_eq!(
            find_observatory_objects(&value).unwrap(),
            vec!["observatory"]
        );
    }

    #[test]
    fn finds_burst_observatory_with_subject_selector() {
        let value: Value = serde_json::from_str(
            r#"{"burstobservatory":{"subjectSelector":["proxy"]}}"#,
        )
        .unwrap();

        assert_eq!(
            find_observatory_objects(&value).unwrap(),
            vec!["burstobservatory"]
        );
    }

    #[test]
    fn rejects_observatory_without_subject_selector() {
        let value: Value =
            serde_json::from_str(r#"{"observatory":{"probeURL":"https://example.com"}}"#)
                .unwrap();

        assert!(find_observatory_objects(&value).is_err());
    }

    #[test]
    fn ignores_nested_observatory_objects() {
        let value: Value = serde_json::from_str(
            r#"{"routing":{"observatory":{"subjectSelector":["proxy"]}}}"#,
        )
        .unwrap();

        assert!(find_observatory_objects(&value).unwrap().is_empty());
    }

    #[test]
    fn recognizes_json_and_jsonc_extensions() {
        assert!(is_json_config(Path::new("default.json")));
        assert!(is_json_config(Path::new("custom.jsonc")));
        assert!(!is_json_config(Path::new("notes.txt")));
    }
}
