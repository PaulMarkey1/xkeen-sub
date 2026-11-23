use std::error::Error;

use crate::backup::BackupSession;
use crate::subscriptions::{load_subscriptions, Subscription};
use crate::terminal::{info, title, warning};
use crate::update_configuration::prepare_configuration_for_test;
use crate::update_merge::subscription_id_number;
use crate::update_subscription::prepare_subscriptions;

type AppResult<T> = Result<T, Box<dyn Error>>;

const UPDATE_LOCK: &str = "/opt/var/run/xkeen-sub-update.lock";
const MAX_DELAY_MS: u64 = 5000;

/// Runs the normal subscription preparation and merge pipeline, then probes
/// the resulting outbound selector without creating a backup or applying files.
pub fn run() -> AppResult<()> {
    let _lock = crate::update_lock::FileLock::acquire(UPDATE_LOCK)?
        .ok_or("Обновление уже выполняется. Дождитесь его завершения и повторите тест.")?;

    println!();
    title("XKEEN-SUB — ТЕСТ ПОДКЛЮЧЕНИЙ");
    warning("Рабочие конфигурации, baseline, чёрный список и основной Xray не изменяются.");

    let mut journal = BackupSession::start_ephemeral("Тест подключений (без применения)")?;
    let store = load_subscriptions()?;
    if store.subscriptions.is_empty() {
        info("Подписок пока нет. Добавьте подписку, чтобы проверить подключения.");
        journal.set_outcome("ЗАВЕРШЕНО", "Подписок нет.", false);
        journal.finish()?;
        return Ok(());
    }

    let mut selected: Vec<&Subscription> = store.subscriptions.iter().collect();
    selected.sort_by_key(|item| subscription_id_number(&item.id).unwrap_or(u64::MAX));
    journal.set_subscriptions(selected.iter().map(|item| {
        match item.name.as_deref().filter(|name| !name.trim().is_empty()) {
            Some(name) => format!("{} ({name})", item.id),
            None => item.id.clone(),
        }
    }).collect());

    journal.event("TEST", "Запущена штатная подготовка подписок без применения изменений.")?;
    let state = prepare_subscriptions(&selected, &mut journal)?;
    if state.downloaded == 0 {
        journal.set_outcome("ОШИБКА ЗАГРУЗКИ", "Не удалось загрузить ни одну подписку.", false);
        journal.finish()?;
        return Err("Не удалось загрузить ни одну подписку. Проверка подключений остановлена.".into());
    }
    if state.processed == 0 {
        journal.set_outcome("ОШИБКА ОБРАБОТКИ", "Ни одна подписка не прошла обработку.", false);
        journal.finish()?;
        return Err("Ни одна подписка не прошла обработку. Проверка подключений остановлена.".into());
    }

    // Match update's in-memory blacklist cleanup, but never persist the pruned list.
    let mut blacklist = crate::blacklist::load()?;
    let removed = blacklist.prune_missing_tags(&state.observed_tags);
    if removed > 0 {
        info(&format!(
            "При подготовке исключены устаревшие записи чёрного списка: {removed}. На диске список не меняется."
        ));
    }

    journal.event("TEST", "Формирование итоговой конфигурации в памяти.")?;
    let prepared = prepare_configuration_for_test(
        &store.subscriptions,
        &state,
        &mut journal,
        &blacklist,
    )?;
    if let Err(message) = &prepared.validation {
        journal.set_outcome("ОШИБКА ПРОВЕРКИ", message, false);
        journal.finish()?;
        return Err(format!("Итоговая конфигурация не прошла проверку Xray: {message}").into());
    }

    let documents: Vec<(String, serde_json::Value)> = prepared.docs.iter()
        .map(|document| (document.file_name.clone(), document.value.clone()))
        .collect();
    let tags = final_observatory_selector(&documents);
    if tags.is_empty() {
        journal.set_outcome("ЗАВЕРШЕНО", "В итоговой конфигурации нет подключений для проверки.", false);
        journal.finish()?;
        info("Итоговый список подключений пуст. Проверять нечего.");
        return Ok(());
    }

    info(&format!(
        "Итоговая конфигурация подготовлена. Подключений для проверки: {}.",
        tags.len()
    ));
    journal.event("TEST", &format!("Подготовлено итоговых подключений: {}.", tags.len()))?;
    crate::xray_probe::test_outbounds(&documents, &tags)?;

    journal.set_outcome(
        "ТЕСТ ЗАВЕРШЁН",
        &format!(
            "Проверены подключения итоговой конфигурации группами по три; порог пинга — {MAX_DELAY_MS} мс. Рабочие файлы не применялись."
        ),
        false,
    );
    journal.finish()?;
    Ok(())
}

fn final_observatory_selector(documents: &[(String, serde_json::Value)]) -> Vec<String> {
    for (_, document) in documents {
        for key in ["observatory", "burstObservatory", "burstobservatory"] {
            if let Some(tags) = document.get(key)
                .and_then(|value| value.get("subjectSelector"))
                .and_then(serde_json::Value::as_array)
            {
                return tags.iter().filter_map(serde_json::Value::as_str)
                    .map(str::to_owned).collect();
            }
        }
    }
    Vec::new()
}
