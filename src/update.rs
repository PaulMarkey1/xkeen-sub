use crate::backup::{BackupSession, BackupSource};
use crate::balancer::{prepare_balancers, BalancerCase, PROGRAM_BALANCER_TAG};
use crate::observatory::synchronize_subject_selectors;
use crate::update_merge::{collect_selector_tags, collect_tags, load_archive, merge_outbounds, subscription_id_number, ArchivePlan};
use crate::update_apply::{self, PlannedOperationKind};
use crate::subscriptions::{load_subscriptions, Subscription};
use crate::system::command_exists;
use crate::terminal::{error, info, success, title, warning, ProgressBar};
use crate::update_config::{collect_api_tags, find_doc, load_config_documents, validate_all, OUTBOUND_FILE, ROUTING_FILE};
use crate::update_plan::build_file_plan;
use crate::update_subscription::prepare_subscriptions;
use crate::update_baseline::apply_baseline_only;
use crate::update_report::{print_baseline_plan, print_comparison, print_configuration_summary, print_file_plan, print_json_section, print_merge_summary, print_prepared_documents, print_subscription_summary, print_updated_outbound, print_xray_test};

use serde_json::Value;
use std::collections::BTreeMap;
use std::error::Error;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

type AppResult<T> = Result<T, Box<dyn Error>>;

pub(crate) struct PreparedSubscription {
    pub(crate) id: String,
    pub(crate) outbounds: Vec<Value>,
}

pub(crate) struct PreparedBaseline {
    pub(crate) id: String,
    pub(crate) original: Vec<u8>,
    pub(crate) path: PathBuf,
}

#[derive(Default)]
pub(crate) struct RunState {
    pub(crate) prepared: Vec<PreparedSubscription>,
    pub(crate) baseline_updates: Vec<PreparedBaseline>,
    pub(crate) baseline_removals: Vec<(String, PathBuf)>,
    pub(crate) failed: Vec<(String, String)>,
    pub(crate) downloaded: usize,
    pub(crate) processed: usize,
}

/// Downloads and analyses subscriptions, prepares documents, validates the
/// complete Xray configuration, backs up originals, then applies the plan with
/// automatic rollback and XKeen state verification.
pub fn run(target_id: Option<&str>) -> AppResult<()> {
    let started_at = Instant::now();
    let mut backup = BackupSession::start("Обновление подписок")?;
    backup.event("UPDATE", "Загрузка и анализ подписок.")?;

    let result = run_inner(target_id, &mut backup);
    if let Err(ref error) = result {
        if backup_status_is_running(&backup) {
            backup.set_outcome("ОШИБКА", &format!("Выполнение update прервано: {error}"), false);
        }
    } else if backup_status_is_running(&backup) {
        backup.set_outcome(
            "ЗАВЕРШЕНО",
            "Обновление завершено без изменения рабочих файлов.",
            true,
        );
    }

    let log_result = backup.finish();
    println!();
    info(&format!("Резервная запись: {}.", backup.id()));
    info(&format!("Время выполнения: {}.", format_elapsed(started_at.elapsed())));
    if let Err(error) = log_result {
        return Err(format!("Не удалось завершить журнал резервной записи: {error}").into());
    }
    result
}

fn backup_status_is_running(backup: &BackupSession) -> bool {
    backup.is_running()
}

fn run_inner(target_id: Option<&str>, backup: &mut BackupSession) -> AppResult<()> {
    println!();
    title("XKEEN-SUB — ОБНОВЛЕНИЕ ПОДПИСОК");
    warning("Перед применением будет создана проверенная резервная копия; при сбое предусмотрен откат.");
    backup.event("UPDATE", "Начата загрузка списка подписок.")?;

    let store = load_subscriptions()?;
    backup.event("UPDATE", &format!("Загружен список подписок: {}.", store.subscriptions.len()))?;
    if store.subscriptions.is_empty() {
        info("Подписок пока нет. Добавьте подписку, чтобы запустить обновление.");
        return Ok(());
    }

    let mut selected: Vec<&Subscription> = match target_id {
        Some(id) => vec![store.subscriptions.iter()
            .find(|item| item.id == id)
            .ok_or_else(|| format!("Подписка {id} не найдена."))?],
        None => store.subscriptions.iter().collect(),
    };
    selected.sort_by_key(|item| subscription_id_number(&item.id).unwrap_or(u64::MAX));
    backup.set_subscriptions(selected.iter().map(|item| {
        match item.name.as_deref() {
            Some(name) if !name.trim().is_empty() => format!("{} ({name})", item.id),
            _ => item.id.clone(),
        }
    }).collect());

    let state = prepare_subscriptions(&selected, backup)?;

    if state.downloaded == 0 {
        println!();
        info("Не удалось получить данные подписок. Проверяем подключение к интернету...");
        if has_internet_connection() {
            warning("Интернет работает, но серверы подписок не ответили. Проверьте ссылки подписок или попробуйте позже.");
        } else {
            error("Нет подключения к интернету. Проверьте соединение и повторите попытку.");
        }
        print_subscription_summary(&state);
        backup.event("ERROR", "Не удалось загрузить ни одну подписку; применение заблокировано.")?;
        backup.set_outcome("ОШИБКА ЗАГРУЗКИ", "Ни одна подписка не была успешно загружена.", false);
        warning("Обновление остановлено. Файлы роутера не менялись.");
        return Ok(());
    }
    if state.processed == 0 {
        print_subscription_summary(&state);
        print_baseline_plan(&state);
        backup.event("ERROR", "Ни одна подписка не прошла обработку; дальнейшая подготовка прекращена.")?;
        backup.set_outcome("ОШИБКА ОБРАБОТКИ", "Ни одна подписка не прошла обработку.", false);
        warning("Не удалось обработать ни одну подписку. Конфигурация роутера осталась без изменений.");
        return Ok(());
    }

    if state.prepared.is_empty() {
        return apply_baseline_only(&state, backup);
    }

    let mut docs = load_config_documents()?;
    backup.event("CONFIG", &format!("Загружено конфигурационных файлов Xray: {}.", docs.len()))?;
    let outbound_index = find_doc(&docs, OUTBOUND_FILE)
        .ok_or_else(|| format!("Не найден файл {OUTBOUND_FILE}."))?;
    let routing_index = find_doc(&docs, ROUTING_FILE)
        .or_else(|| docs.iter().position(|doc| doc.value.get("routing").is_some()))
        .ok_or("Не найдена конфигурация routing.")?;

    let working = docs[outbound_index].value.get("outbounds")
        .and_then(Value::as_array).cloned()
        .ok_or("В рабочем Outbound отсутствует массив outbounds.")?;
    info(&format!("Рабочий Outbound: {} ({} подключений).",
        docs[outbound_index].path.display(), working.len()));
    info(&format!("Routing: {}.", docs[routing_index].path.display()));
    print_json_section("РАБОЧИЙ OUTBOUND", &Value::Array(working.clone()));

    let api_tags = collect_api_tags(&docs);
    let updated_groups = updated_groups(&state.prepared);
    let mut archive = load_archive()?;
    let (final_outbounds, replacements, conflicts) =
        merge_outbounds(&working, &store.subscriptions, &updated_groups, &mut archive)?;
    let all_tags = collect_tags(&final_outbounds)?;
    let tags = collect_selector_tags(&final_outbounds, &api_tags)?;
    print_json_section("ИТОГОВЫЙ OUTBOUND", &Value::Array(final_outbounds.clone()));

    let mut outbound_doc = docs[outbound_index].value.clone();
    outbound_doc.as_object_mut()
        .ok_or("Корень Outbound должен быть JSON-объектом.")?
        .insert("outbounds".to_owned(), Value::Array(final_outbounds.clone()));
    docs[outbound_index].value = outbound_doc;

    let balancer = prepare_balancers(
        &mut docs[routing_index].value, &working, &tags, &api_tags
    )?;

    let mut obs_docs: Vec<(String, Value)> =
        docs.iter().map(|doc| (doc.file_name.clone(), doc.value.clone())).collect();
    let obs_updates = synchronize_subject_selectors(&mut obs_docs, &tags)?;
    for (name, value) in obs_docs {
        if let Some(doc) = docs.iter_mut().find(|doc| doc.file_name == name) {
            doc.value = value;
        }
    }

    backup.event("MERGE", &format!(
        "Объединение Outbound завершено. Замен пользовательских подключений: {}; конфликтов: {}; новых записей архива: {}.",
        replacements.len(), conflicts.len(), archive.added
    ))?;
    for item in &replacements {
        backup.event("MERGE", item)?;
    }
    for item in &conflicts {
        backup.event("MERGE", item)?;
    }
    backup.event("BALANCER", &format!(
        "Сценарий: {:?}; создан: {}; обновлён существующий: {}; перенаправлено routing rules: {}.",
        balancer.case, balancer.created, balancer.updated_existing, balancer.transformed_rules
    ))?;
    backup.event("OBSERVATORY", &format!("Подготовлено обновлений subjectSelector: {}.", obs_updates.len()))?;

    backup.event("CONFIG", "Подготовка конфигураций завершена. Запускается обязательная проверка Xray.")?;
    let validation = validate_all(&docs, &final_outbounds, &all_tags, &balancer);

    print_subscription_summary(&state);
    print_updated_outbound(&state.prepared);
    print_merge_summary(&replacements, &conflicts, &archive);
    print_configuration_summary(&docs, outbound_index, routing_index, &obs_updates, &balancer, &all_tags);
    print_xray_test(&validation);
    print_baseline_plan(&state);
    print_prepared_documents(&docs);

    let mut no_file_changes = false;
    if let Err(message) = &validation {
        backup.event("XRAY", &format!("Проверка Xray не пройдена: {message}"))?;
        backup.set_outcome("ЗАБЛОКИРОВАНО", "Проверка Xray не пройдена. План применения не формировался, резервное копирование файлов не выполнялось.", false);
    } else {
        backup.event("XRAY", "Проверка Xray завершилась успешно.")?;
        let plan = build_file_plan(&docs, &state, &archive)?;
        no_file_changes = plan.is_empty();
        print_file_plan(&plan);
        backup.set_plan_count(plan.len());
        backup.event("PLAN", &format!("Сформирован итоговый план: {} операций.", plan.len()))?;
        for operation in &plan {
            let action = match operation.kind {
                PlannedOperationKind::Create => "Создание",
                PlannedOperationKind::Replace => "Замена",
                PlannedOperationKind::Delete => "Удаление",
            };
            backup.event("PLAN", &format!("{action}: {} | category={}", serde_json::to_string(&operation.path.display().to_string())?, operation.category))?;
        }
        let backup_sources: Vec<BackupSource> = plan.iter()
            .filter(|operation| operation.kind != PlannedOperationKind::Create)
            .map(|operation| BackupSource {
                path: operation.path.clone(),
                category: operation.category.to_owned(),
            })
            .collect();
        let progress_total = if plan.is_empty() { 0 } else { backup_sources.len() + plan.len() * 3 + 2 };
        let mut progress = ProgressBar::new(progress_total);
        match backup.backup_files_with_progress(&backup_sources, &mut progress) {
            Ok(saved) => {
                backup.event("BACKUP", &format!("Резервирование завершено: {saved} файлов сохранено и проверено."))?;
                match update_apply::execute(&plan, backup, &mut progress) {
                    Ok(()) => {
                        progress.finish("Все операции завершены");
                        backup.set_outcome(
                            "УСПЕШНО",
                            if no_file_changes {
                                "Изменений файлов не требовалось. Перезапуск XKeen не выполнялся."
                            } else {
                                "Изменения применены, итоговое состояние файлов проверено, XKeen запущен и его статус подтверждён."
                            },
                            true,
                        );
                    }
                    Err(error) => {
                        progress.stop("Применение остановлено");
                        backup.set_outcome(
                            "ОШИБКА ПРИМЕНЕНИЯ",
                            &format!("Применение не завершено. Результат отката и проверки XKeen записан в журнал: {error}"),
                            false,
                        );
                        return Err(error);
                    }
                }
            }
            Err(error) => {
                progress.stop("Резервную копию не удалось создать");
                backup.event("ERROR", &format!("Резервирование не пройдено: {error}"))?;
                backup.set_outcome("ОШИБКА РЕЗЕРВИРОВАНИЯ", &format!("Применение заблокировано: {error}"), false);
                return Err(error);
            }
        }
    }

    println!();
    title("ОБНОВЛЕНИЕ ЗАВЕРШЕНО");
    if balancer.created {
        success(&format!(
            "Подготовлено создание отдельного балансировщика программы с Tag {}.",
            PROGRAM_BALANCER_TAG
        ));
        if balancer.case != BalancerCase::NoExistingBalancers {
            warning("Для нового балансировщика routing rule потребуется настроить самостоятельно; существующие пользовательские балансировщики и их правила не изменялись.");
        }
    }
    if validation.is_err() {
        error("Проверка конфигурации не пройдена. Изменения не применялись.");
    } else if no_file_changes {
        success("Конфигурация уже актуальна. Файлы не менялись, перезапуск XKeen не потребовался.");
    } else {
        success("Готово! Файлы обновлены и проверены, XKeen запущен.");
    }
    Ok(())
}

fn format_elapsed(duration: Duration) -> String {
    let total_seconds = duration.as_secs();
    let hours = total_seconds / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;

    let mut parts = Vec::new();
    if hours > 0 {
        parts.push(format!("{hours} {}", russian_plural(hours, "час", "часа", "часов")));
    }
    if minutes > 0 {
        parts.push(format!("{minutes} {}", russian_plural(minutes, "минута", "минуты", "минут")));
    }
    if seconds > 0 || parts.is_empty() {
        parts.push(format!("{seconds} {}", russian_plural(seconds, "секунда", "секунды", "секунд")));
    }
    parts.join(" ")
}

fn russian_plural(value: u64, one: &'static str, few: &'static str, many: &'static str) -> &'static str {
    let last_two = value % 100;
    let last = value % 10;
    if (11..=14).contains(&last_two) {
        // The returned literals have static lifetime.
        many
    } else {
        match last {
            1 => one,
            2..=4 => few,
            _ => many,
        }
    }
}

#[cfg(test)]
mod elapsed_time_tests {
    use super::format_elapsed;
    use std::time::Duration;

    #[test]
    fn formats_only_non_zero_units() {
        assert_eq!(format_elapsed(Duration::from_secs(5)), "5 секунд");
        assert_eq!(format_elapsed(Duration::from_secs(65)), "1 минута 5 секунд");
        assert_eq!(format_elapsed(Duration::from_secs(3600)), "1 час");
        assert_eq!(format_elapsed(Duration::from_secs(3661)), "1 час 1 минута 1 секунда");
    }

    #[test]
    fn formats_russian_plural_forms() {
        assert_eq!(format_elapsed(Duration::from_secs(2)), "2 секунды");
        assert_eq!(format_elapsed(Duration::from_secs(11)), "11 секунд");
        assert_eq!(format_elapsed(Duration::from_secs(21)), "21 секунда");
    }

    #[test]
    fn zero_duration_still_shows_seconds() {
        assert_eq!(format_elapsed(Duration::ZERO), "0 секунд");
    }
}

fn updated_groups(prepared: &[PreparedSubscription]) -> BTreeMap<u64, Vec<Value>> {
    prepared.iter().filter_map(|item| {
        subscription_id_number(&item.id).map(|n| (n, item.outbounds.clone()))
    }).collect()
}

fn has_internet_connection() -> bool {
    const URLS: [&str; 2] = ["https://www.cloudflare.com/", "https://www.google.com/"];
    if command_exists("curl") {
        return URLS.iter().any(|url| Command::new("curl")
            .args(["--location", "--silent", "--output", "/dev/null", "--connect-timeout", "5", "--max-time", "10", url])
            .status().map(|s| s.success()).unwrap_or(false));
    }
    if command_exists("wget") {
        return URLS.iter().any(|url| Command::new("wget")
            .args(["--quiet", "--spider", "--timeout=5", "--tries=1", url])
            .status().map(|s| s.success()).unwrap_or(false));
    }
    false
}


