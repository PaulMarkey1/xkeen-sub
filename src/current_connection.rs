use serde_json::Value;
use std::collections::BTreeSet;
use std::error::Error;
use std::fs;
use std::path::Path;
use std::process::Command;

use crate::terminal::{info, title};

type AppResult<T> = Result<T, Box<dyn Error>>;

const CONFIG_DIR: &str = "/opt/etc/xray/configs";
const XRAY_BIN: &str = "/opt/sbin/xray";

pub fn run() -> AppResult<()> {
    let configs = load_configs()?;
    let server = find_api_server(&configs)?;
    let balancer = find_active_balancer(&configs)?;
    let tag = query_balancer(&server, &balancer)?;

    title("ТЕКУЩЕЕ ПОДКЛЮЧЕНИЕ");
    info(&format!("Тэг: {tag}"));
    Ok(())
}

fn load_configs() -> AppResult<Vec<Value>> {
    let dir = Path::new(CONFIG_DIR);
    if !dir.is_dir() {
        return Err(format!("Каталог конфигурации Xray не найден: {}", dir.display()).into());
    }

    let mut paths = fs::read_dir(dir)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && path.extension().and_then(|ext| ext.to_str())
                    .map(|ext| ext.eq_ignore_ascii_case("json") || ext.eq_ignore_ascii_case("jsonc"))
                    .unwrap_or(false)
        })
        .collect::<Vec<_>>();
    paths.sort();

    if paths.is_empty() {
        return Err("В каталоге Xray не найдены файлы JSON/JSONC.".into());
    }

    paths.into_iter().map(|path| {
        let content = fs::read_to_string(&path)
            .map_err(|error| format!("Не удалось прочитать {}: {error}", path.display()))?;
        json5::from_str(&content)
            .map_err(|error| format!("Не удалось разобрать {}: {error}", path.display()).into())
    }).collect()
}

fn find_api_server(configs: &[Value]) -> AppResult<String> {
    let api = configs.iter().find_map(|config| config.get("api"))
        .ok_or("В конфигурации Xray не включён API. Для просмотра текущего подключения требуется API с RoutingService.")?;

    let services = api.get("services").and_then(Value::as_array)
        .ok_or("В настройках Xray API не найден список services.")?;
    if !services.iter().any(|service| service.as_str() == Some("RoutingService")) {
        return Err("В Xray API не включён RoutingService. Добавьте его в api.services.".into());
    }

    if let Some(listen) = api.get("listen").and_then(Value::as_str) {
        return validate_server_address(listen);
    }

    // Legacy API configuration uses an inbound whose tag matches api.tag.
    let api_tag = api.get("tag").and_then(Value::as_str)
        .ok_or("В настройках Xray API отсутствует tag, необходимый для поиска API inbound.")?;

    for config in configs {
        let Some(inbounds) = config.get("inbounds").and_then(Value::as_array) else {
            continue;
        };
        for inbound in inbounds {
            if inbound.get("tag").and_then(Value::as_str) != Some(api_tag) {
                continue;
            }
            let port = inbound.get("port").and_then(Value::as_u64)
                .ok_or("У API inbound не задан корректный port.")?;
            let host = inbound.get("listen").and_then(Value::as_str).unwrap_or("127.0.0.1");
            return validate_server_address(&format!("{host}:{port}"));
        }
    }

    Err("Не удалось определить адрес Xray API: нет api.listen и не найден inbound с tag API.".into())
}

fn validate_server_address(address: &str) -> AppResult<String> {
    let address = address.trim();
    if address.is_empty() || !address.rsplit_once(':').map(|(_, port)| port.parse::<u16>().is_ok()).unwrap_or(false) {
        return Err(format!("Некорректный адрес Xray API: {address}").into());
    }
    Ok(address.to_owned())
}

fn find_active_balancer(configs: &[Value]) -> AppResult<String> {
    let mut configured = BTreeSet::new();
    let mut referenced = BTreeSet::new();

    for config in configs {
        if let Some(balancers) = config.get("routing").and_then(|routing| routing.get("balancers")).and_then(Value::as_array) {
            for balancer in balancers {
                if let Some(tag) = balancer.get("tag").and_then(Value::as_str) {
                    configured.insert(tag.to_owned());
                }
            }
        }
        if let Some(rules) = config.get("routing").and_then(|routing| routing.get("rules")).and_then(Value::as_array) {
            for rule in rules {
                if let Some(tag) = rule.get("balancerTag").and_then(Value::as_str) {
                    referenced.insert(tag.to_owned());
                }
            }
        }
    }

    let active = referenced.intersection(&configured).cloned().collect::<Vec<_>>();
    match active.as_slice() {
        [tag] => Ok(tag.clone()),
        [] => match configured.iter().collect::<Vec<_>>().as_slice() {
            [tag] => Ok((*tag).clone()),
            [] => Err("В конфигурации Xray не найден ни один балансировщик.".into()),
            _ => Err("Не удалось однозначно определить балансировщик: ни один из настроенных балансировщиков не указан в routing.rules.".into()),
        },
        tags => Err(format!(
            "В routing.rules используются несколько балансировщиков ({}). Нельзя определить один текущий тэг без контекста конкретного запроса.",
            tags.join(", ")
        ).into()),
    }
}

fn query_balancer(server: &str, balancer: &str) -> AppResult<String> {
    if !Path::new(XRAY_BIN).is_file() {
        return Err(format!("Исполняемый файл Xray не найден: {XRAY_BIN}").into());
    }

    let output = Command::new(XRAY_BIN)
        .args(["api", "bi", &format!("--server={server}"), "--timeout=3", "--json", balancer])
        .output()
        .map_err(|error| format!("Не удалось запустить Xray API: {error}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let details = if !stderr.trim().is_empty() { stderr.trim() } else { stdout.trim() };
        let reason = if details.is_empty() {
            format!("код завершения {}", output.status)
        } else {
            details.to_owned()
        };
        return Err(format!("Не удалось получить состояние балансировщика {balancer} через Xray API ({reason}). Проверьте, запущен ли Xray и доступен ли API.").into());
    }

    let response: Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("Xray API вернул неожиданный ответ: {error}"))?;
    parse_selected_tag(&response)
        .ok_or_else(|| format!("Xray API не сообщил выбранный outbound для балансировщика {balancer}.").into())
}

fn parse_selected_tag(response: &Value) -> Option<String> {
    let balancer = response.get("balancer").unwrap_or(response);
    if let Some(target) = balancer.get("override")
        .and_then(|value| value.get("target"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    {
        return Some(target.to_owned());
    }

    balancer.get("principleTarget")
        .and_then(|value| value.get("tag"))
        .and_then(Value::as_array)
        .and_then(|tags| tags.first())
        .and_then(Value::as_str)
        .filter(|tag| !tag.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::parse_selected_tag;
    use serde_json::json;

    #[test]
    fn selected_tag_uses_strategy_first_target() {
        let response = json!({
            "balancer": {
                "principleTarget": { "tag": ["sub1--node-a", "sub1--node-b"] }
            }
        });
        assert_eq!(parse_selected_tag(&response).as_deref(), Some("sub1--node-a"));
    }

    #[test]
    fn manual_override_has_priority_over_strategy() {
        let response = json!({
            "balancer": {
                "override": { "target": "sub2--forced" },
                "principleTarget": { "tag": ["sub1--node-a"] }
            }
        });
        assert_eq!(parse_selected_tag(&response).as_deref(), Some("sub2--forced"));
    }

    #[test]
    fn empty_balancer_response_has_no_selected_tag() {
        assert_eq!(parse_selected_tag(&json!({"balancer": {}})), None);
    }
}
