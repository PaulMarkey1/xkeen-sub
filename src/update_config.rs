use serde_json::Value;
use std::collections::HashSet;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

use crate::balancer::BalancerPreparation;
use crate::xray_test::{test_config_set_detailed, XrayConfigInput};

type AppResult<T> = Result<T, Box<dyn Error>>;

pub(crate) const CONFIG_DIR: &str = "/opt/etc/xray/configs";

pub(crate) const OUTBOUND_FILE: &str = "04_outbounds.json";
pub(crate) const ROUTING_FILE: &str = "05_routing.json";

pub(crate) struct ConfigDocument {
    pub(crate) file_name: String,
    pub(crate) path: PathBuf,
    pub(crate) original: String,
    pub(crate) original_value: Value,
    pub(crate) value: Value,
    pub(crate) is_new: bool,
}

pub(crate) fn load_config_documents() -> AppResult<Vec<ConfigDocument>> {
    let dir = Path::new(CONFIG_DIR);
    if !dir.is_dir() {
        return Err(format!("Каталог Xray не найден: {}", dir.display()).into());
    }
    let mut paths = Vec::new();
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        let hidden = path.file_name().and_then(|n| n.to_str()).map(|n| n.starts_with('.')).unwrap_or(false);
        if path.is_file() && !hidden && is_json_config(&path) {
            paths.push(path);
        }
    }
    paths.sort();

    let mut docs = Vec::new();
    for path in paths {
        let original = fs::read_to_string(&path)?;
        let value: Value = json5::from_str(&original)
            .map_err(|err| format!("Ошибка разбора {}: {err}", path.display()))?;
        let file_name = path.file_name().and_then(|n| n.to_str())
            .ok_or("Не удалось определить имя конфигурационного файла.")?.to_owned();
        docs.push(ConfigDocument {
            file_name,
            path,
            original,
            original_value: value.clone(),
            value,
            is_new: false,
        });
    }
    Ok(docs)
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

pub(crate) fn find_doc(docs: &[ConfigDocument], name: &str) -> Option<usize> {
    docs.iter().position(|doc| doc.file_name == name)
}

pub(crate) fn collect_api_tags(docs: &[ConfigDocument]) -> HashSet<String> {
    docs.iter().filter_map(|doc| doc.value.get("api"))
        .filter_map(|api| api.get("tag")).filter_map(Value::as_str)
        .map(str::to_owned).collect()
}

pub(crate) fn validate_all(
    docs: &[ConfigDocument],
    outbounds: &[Value],
    tags: &[String],
    balancer: &BalancerPreparation,
) -> Result<(), String> {
    let routing = docs.iter().find(|d| d.file_name == ROUTING_FILE)
        .or_else(|| docs.iter().find(|d| d.value.get("routing").is_some()))
        .ok_or_else(|| "Не найдена конфигурация routing.".to_owned())?;
    let balancer_tags: HashSet<&str> = routing.value.get("routing")
        .and_then(|v| v.get("balancers")).and_then(Value::as_array).into_iter()
        .flatten().filter_map(|v| v.get("tag").and_then(Value::as_str)).collect();
    if !balancer_tags.contains(balancer.tag.as_str()) {
        return Err(format!("Балансировщик {} не найден после подготовки.", balancer.tag));
    }
    if outbounds.len() != tags.len() { return Err("Не совпало количество Outbound и Tag.".to_owned()); }

    let inputs: Vec<XrayConfigInput> = docs.iter().map(|doc| {
        let content = serde_json::to_string_pretty(&doc.value).unwrap_or_else(|_| doc.original.clone());
        XrayConfigInput::new(doc.file_name.clone(), content)
    }).collect();
    match test_config_set_detailed(&inputs) {
        Ok(output) => {
            if !output.stdout.trim().is_empty() {
                println!("{}", output.stdout.trim_end());
            }
            if !output.stderr.trim().is_empty() {
                eprintln!("{}", output.stderr.trim_end());
            }
            Ok(())
        }
        Err(crate::xray_test::XrayTestError::Process(failure)) => {
            let code = failure.exit_code.map(|value| value.to_string()).unwrap_or_else(|| failure.process_status.clone());
            let details = failure.combined_diagnostics();
            if details.is_empty() {
                Err(format!("Xray config test завершился с ошибкой (код {code})."))
            } else {
                Err(format!("Xray config test завершился с ошибкой (код {code}):\n{details}"))
            }
        }
        Err(other) => Err(format!("Xray config test: {other}")),
    }
}

