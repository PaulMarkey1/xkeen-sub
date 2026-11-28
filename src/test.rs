use serde_json::Value;
use std::error::Error;

use crate::terminal::{title, warning};
use crate::update_config::{load_config_documents, OUTBOUND_FILE};
use crate::update_lock::FileLock;

type AppResult<T> = Result<T, Box<dyn Error>>;

const UPDATE_LOCK: &str = "/opt/var/run/xkeen-sub-update.lock";

/// Measures every usable outbound in the currently active 04_outbounds.json.
/// No subscription download, parsing, update preparation, or config writes occur.
pub fn run() -> AppResult<()> {
    let _lock = FileLock::acquire(UPDATE_LOCK)?
        .ok_or("Обновление уже выполняется. Дождитесь его завершения и повторите тест.")?;

    println!();
    title("XKEEN-SUB — ТЕСТ ПОДКЛЮЧЕНИЙ");
    warning("Используются текущие рабочие конфигурации. Файлы, подписки, чёрный список и основной Xray не изменяются.");

    let docs = load_config_documents()?;
    let outbound_doc = docs.iter()
        .find(|doc| doc.file_name == OUTBOUND_FILE)
        .ok_or_else(|| format!("Рабочий файл {} не найден.", OUTBOUND_FILE))?;

    let outbounds = outbound_doc.value.get("outbounds")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("В {} отсутствует корректный массив outbounds.", OUTBOUND_FILE))?;

    let mut tags = Vec::new();
    for outbound in outbounds {
        let Some(tag) = outbound.get("tag").and_then(Value::as_str) else {
            continue;
        };
        let protocol = outbound.get("protocol").and_then(Value::as_str).unwrap_or("");
        if tag == "api" || matches!(protocol, "freedom" | "blackhole" | "dns" | "loopback") {
            continue;
        }
        if !tags.iter().any(|existing: &String| existing == tag) {
            tags.push(tag.to_owned());
        }
    }

    if tags.is_empty() {
        println!("В рабочем {} нет подключений для проверки.", OUTBOUND_FILE);
        return Ok(());
    }

    let documents: Vec<(String, Value)> = docs.into_iter()
        .map(|doc| (doc.file_name, doc.value))
        .collect();

    println!(
        "Подключения загружены непосредственно из рабочего {}: {}.",
        OUTBOUND_FILE,
        tags.len()
    );
    crate::speed_test::test_outbounds_speed(&documents, &tags).map(|_| ())
}
