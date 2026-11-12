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
const OBSERVATORY_TEMPLATE: &str =
    "\n  \"observatory\": {\n    \"subjectSelector\": []\n  }";

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

        for key in find_observatory_objects(&document) {
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

    ensure_observatory_template(config_dir)
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

/// Only a top-level Observatory object with subjectSelector counts.
/// Incomplete or malformed values do not prevent checking other files.
fn find_observatory_objects(document: &Value) -> Vec<&'static str> {
    let Some(root) = document.as_object() else {
        return Vec::new();
    };

    ["observatory", "burstobservatory"]
        .into_iter()
        .filter(|key| {
            root.get(*key)
                .and_then(Value::as_object)
                .map(|object| object.contains_key("subjectSelector"))
                .unwrap_or(false)
        })
        .collect()
}

fn ensure_observatory_template(config_dir: &Path) -> AppResult<()> {
    let path: PathBuf = config_dir.join(OBSERVATORY_FILE);

    match fs::read_to_string(&path) {
        Ok(existing) => {
            let document: Value = json5::from_str(&existing).map_err(|error| {
                format!(
                    "Не удалось добавить Observatory в {}: файл содержит некорректный JSON: {error}",
                    path.display()
                )
            })?;

            if !document.is_object() {
                return Err(format!(
                    "Не удалось добавить Observatory в {}: корневое значение должно быть JSON-объектом.",
                    path.display()
                )
                .into());
            }

            let updated = insert_observatory_at_root(&existing, &document)?;
            fs::write(&path, updated).map_err(|error| {
                format!("Не удалось обновить файл {}: {error}", path.display())
            })?;

            success(&format!(
                "Объекты Observatory не найдены. Шаблон добавлен в начало файла {}; прежние настройки сохранены.",
                path.display()
            ));
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o644)
                .open(&path)
                .map_err(|error| {
                    format!("Не удалось создать файл {}: {error}", path.display())
                })?;

            let new_file = format!("{{{}\n}}\n", OBSERVATORY_TEMPLATE);
            if let Err(error) = file.write_all(new_file.as_bytes()) {
                let _ = fs::remove_file(&path);
                return Err(format!("Не удалось записать файл {}: {error}", path.display()).into());
            }

            success(&format!(
                "Объекты Observatory не найдены. Создан файл {}.",
                path.display()
            ));
            Ok(())
        }
        Err(error) => Err(format!(
            "Не удалось проверить файл {}: {error}",
            path.display()
        )
        .into()),
    }
}

/// Inserts the template immediately after the opening brace of the root
/// object. Existing members and their formatting are retained.
fn insert_observatory_at_root(existing: &str, document: &Value) -> AppResult<String> {
    let first = existing
        .find('{')
        .ok_or("В файле отсутствует открывающая фигурная скобка")?;
    if !existing[..first].trim().is_empty() {
        return Err("Перед корневым JSON-объектом обнаружен посторонний текст".into());
    }

    let after_open = first + 1;
    let remainder = &existing[after_open..];
    let first_non_whitespace = remainder
        .char_indices()
        .find(|(_, character)| !character.is_whitespace());

    let Some((offset, character)) = first_non_whitespace else {
        return Err("Файл содержит незавершённый JSON-объект".into());
    };

    let insertion_point = after_open + offset;
    if character == '}' {
        if !document.as_object().map(|object| object.is_empty()).unwrap_or(false) {
            return Err("Не удалось определить начало содержимого корневого объекта".into());
        }

        let mut updated = String::with_capacity(existing.len() + OBSERVATORY_TEMPLATE.len() + 2);
        updated.push_str(&existing[..after_open]);
        updated.push_str(OBSERVATORY_TEMPLATE);
        updated.push_str(&existing[insertion_point..]);
        return Ok(updated);
    }

    let mut updated = String::with_capacity(existing.len() + OBSERVATORY_TEMPLATE.len() + 2);
    updated.push_str(&existing[..after_open]);
    updated.push_str(OBSERVATORY_TEMPLATE);
    updated.push_str(",\n");
    updated.push_str(&existing[insertion_point..]);
    Ok(updated)
}

#[cfg(test)]
mod tests {
    use super::{find_observatory_objects, insert_observatory_at_root, is_json_config};
    use serde_json::Value;
    use std::path::Path;

    #[test]
    fn finds_observatory_with_subject_selector() {
        let value: Value = serde_json::from_str(
            r#"{"observatory":{"subjectSelector":["proxy"]}}"#,
        )
        .unwrap();
        assert_eq!(find_observatory_objects(&value), vec!["observatory"]);
    }

    #[test]
    fn finds_burst_observatory_with_subject_selector() {
        let value: Value = serde_json::from_str(
            r#"{"burstobservatory":{"subjectSelector":["proxy"]}}"#,
        )
        .unwrap();
        assert_eq!(find_observatory_objects(&value), vec!["burstobservatory"]);
    }

    #[test]
    fn ignores_observatory_without_subject_selector() {
        let value: Value = serde_json::from_str(
            r#"{"observatory":{"probeURL":"https://example.com"}}"#,
        )
        .unwrap();
        assert!(find_observatory_objects(&value).is_empty());
    }

    #[test]
    fn ignores_nested_observatory_objects() {
        let value: Value = serde_json::from_str(
            r#"{"routing":{"observatory":{"subjectSelector":["proxy"]}}}"#,
        )
        .unwrap();
        assert!(find_observatory_objects(&value).is_empty());
    }

    #[test]
    fn inserts_template_before_existing_root_members() {
        let original = "{\n  \"log\": { \"loglevel\": \"warning\" }\n}\n";
        let parsed: Value = json5::from_str(original).unwrap();
        let updated = insert_observatory_at_root(original, &parsed).unwrap();
        let expected = "{\n  \"observatory\": {\n    \"subjectSelector\": []\n  },\n  \"log\": { \"loglevel\": \"warning\" }\n}\n";
        assert_eq!(updated, expected);
        let _: Value = json5::from_str(&updated).unwrap();
    }

    #[test]
    fn inserts_template_into_empty_root_object() {
        let original = "{}";
        let parsed: Value = json5::from_str(original).unwrap();
        let updated = insert_observatory_at_root(original, &parsed).unwrap();
        let _: Value = json5::from_str(&updated).unwrap();
        assert!(updated.contains("\"subjectSelector\": []"));
    }

    #[test]
    fn recognizes_json_and_jsonc_extensions() {
        assert!(is_json_config(Path::new("default.json")));
        assert!(is_json_config(Path::new("custom.jsonc")));
        assert!(!is_json_config(Path::new("notes.txt")));
    }
}
