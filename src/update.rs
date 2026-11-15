use crate::backup::{BackupSession, BackupSource};
use crate::balancer::{prepare_balancers, BalancerCase, BalancerPreparation, PROGRAM_BALANCER_TAG};
use crate::observatory::{synchronize_subject_selectors, ObservatoryUpdate};
use crate::outbound_compare::{compare_subscription, ComparisonReport};
use crate::subscription_parser::parse_subscription;
use crate::update_merge::{collect_tags, load_archive, merge_outbounds, subscription_id_number, ArchivePlan, USER_ARCHIVE};
use crate::update_apply::{self, PlannedFileOperation, PlannedOperationKind};
use crate::subscription_download::download_subscription;
use crate::subscriptions::{load_subscriptions, Subscription};
use crate::terminal::{error, info, success, title, warning};
use crate::xray_test::{test_config_set_detailed, XrayConfigInput};

use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

type AppResult<T> = Result<T, Box<dyn Error>>;

const BASELINE_DIR: &str = "/opt/etc/xkeen-sub/baseline";
const CONFIG_DIR: &str = "/opt/etc/xray/configs";
const OUTBOUND_FILE: &str = "04_outbounds.json";
const ROUTING_FILE: &str = "05_routing.json";

struct ConfigDocument {
    file_name: String,
    path: PathBuf,
    original: String,
    original_value: Value,
    value: Value,
}

struct PreparedSubscription {
    id: String,
    outbounds: Vec<Value>,
}

struct PreparedBaseline {
    id: String,
    original: Vec<u8>,
    path: PathBuf,
}

#[derive(Default)]
struct RunState {
    prepared: Vec<PreparedSubscription>,
    baseline_updates: Vec<PreparedBaseline>,
    baseline_removals: Vec<(String, PathBuf)>,
    failed: Vec<(String, String)>,
    downloaded: usize,
    processed: usize,
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
        info("Список подписок пуст.");
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
    let mut state = RunState::default();
    for subscription in selected {
        backup.event("SUBSCRIPTION", &format!("Начата обработка {}.", subscription.id))?;
        prepare_subscription(subscription, &mut state, backup)?;
        backup.event(
            "SUBSCRIPTION",
            &format!("Обработка {} завершена. Загружено: {}, подготовлено изменений: {}, ошибок: {}.",
                subscription.id, state.downloaded, state.prepared.len(), state.failed.len()),
        )?;
    }
    backup.set_updated_subscriptions(state.prepared.iter().map(|item| item.id.clone()).collect());

    if state.downloaded == 0 {
        println!();
        info("Ни одна подписка не была успешно загружена. Проверка интернет-соединения...");
        if has_internet_connection() {
            warning("Интернет доступен, но серверы подписок не ответили.");
        } else {
            error("Отсутствует интернет-соединение.");
        }
        print_subscription_summary(&state);
        backup.event("ERROR", "Не удалось загрузить ни одну подписку; применение заблокировано.")?;
        backup.set_outcome("ОШИБКА ЗАГРУЗКИ", "Ни одна подписка не была успешно загружена.", false);
        warning("Подготовка конфигурации прекращена. Рабочие файлы не изменены.");
        return Ok(());
    }
    if state.processed == 0 {
        print_subscription_summary(&state);
        print_baseline_plan(&state);
        backup.event("ERROR", "Ни одна подписка не прошла обработку; дальнейшая подготовка прекращена.")?;
        backup.set_outcome("ОШИБКА ОБРАБОТКИ", "Ни одна подписка не прошла обработку.", false);
        warning("Ни одна подписка не прошла обработку. Подготовка конфигурации прекращена.");
        return Ok(());
    }

    let updated_groups = updated_groups(&state.prepared);
    let mut archive = load_archive()?;
    let (final_outbounds, replacements, conflicts) =
        merge_outbounds(&working, &store.subscriptions, &updated_groups, &mut archive)?;
    let tags = collect_tags(&final_outbounds)?;
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
    let validation = validate_all(&docs, &final_outbounds, &tags, &balancer);

    print_subscription_summary(&state);
    print_updated_outbound(&state.prepared);
    print_merge_summary(&replacements, &conflicts, &archive);
    print_configuration_summary(&docs, outbound_index, routing_index, &obs_updates, &balancer, &tags);
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
        match backup.backup_files(&backup_sources) {
            Ok(saved) => {
                backup.event("BACKUP", &format!("Резервирование завершено: {saved} файлов сохранено и проверено."))?;
                match update_apply::execute(&plan, backup) {
                    Ok(()) => {
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
        error("Итоговая проверка блокирует будущее применение.");
    } else if no_file_changes {
        success("Изменений файлов не требуется; перезапуск XKeen не выполнялся.");
    } else {
        success("Обновление выполнено; файлы и состояние XKeen проверены.");
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
mod subscription_comparison_tests {
    use super::{baseline_path, byte_contents_equal, serialize_outbounds};
    use crate::subscriptions::Subscription;
    use serde_json::json;
    use std::path::Path;

    #[test]
    fn baseline_path_uses_dedicated_directory_and_subscription_id() {
        let subscription = Subscription {
            id: "sub3".to_owned(),
            name: Some("Netherlands".to_owned()),
            url: "https://example.test/sub".to_owned(),
            prefix: "sub3--Netherlands--".to_owned(),
        };

        assert_eq!(
            baseline_path(&subscription).unwrap(),
            Path::new("/opt/etc/xkeen-sub/baseline/sub3--Netherlands.json")
        );
    }

    #[test]
    fn baseline_path_rejects_path_separators_in_subscription_name() {
        let subscription = Subscription {
            id: "sub1".to_owned(),
            name: Some("../outside".to_owned()),
            url: "https://example.test/sub".to_owned(),
            prefix: "sub1--outside--".to_owned(),
        };

        assert!(baseline_path(&subscription).is_err());
    }

    #[test]
    fn byte_comparison_detects_identical_and_different_source_files() {
        assert!(byte_contents_equal(Some(b"subscription"), b"subscription"));
        assert!(!byte_contents_equal(Some(b"subscription"), b"subscription\n"));
        assert!(!byte_contents_equal(None, b"subscription"));
    }

    #[test]
    fn serialized_outbounds_are_byte_identical_for_identical_parsed_data() {
        let outbounds = vec![
            json!({"tag":"one","protocol":"vless"}),
            json!({"tag":"two","protocol":"trojan"}),
        ];

        let first = serialize_outbounds(&outbounds).unwrap();
        let second = serialize_outbounds(&outbounds).unwrap();

        assert_eq!(first, second);
    }

    #[test]
    fn serialized_outbounds_differ_when_connection_parameters_change() {
        let previous = vec![json!({"tag":"one","protocol":"vless","port":443})];
        let current = vec![json!({"tag":"one","protocol":"vless","port":8443})];

        assert_ne!(
            serialize_outbounds(&previous).unwrap(),
            serialize_outbounds(&current).unwrap()
        );
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

fn prepare_subscription(
    subscription: &Subscription,
    state: &mut RunState,
    backup: &mut BackupSession,
) -> AppResult<()> {
    println!();
    title(&format!("Подписка {}", subscription.id));
    info("Загрузка...");
    let downloaded = match download_subscription(subscription) {
        Ok(bytes) => {
            state.downloaded += 1;
            success(&format!("Получено {} байт.", bytes.len()));
            backup.event("DOWNLOAD", &format!("{}: получено {} байт.", subscription.id, bytes.len()))?;
            bytes
        }
        Err(err) => {
            let message = err.to_string();
            state.failed.push((subscription.id.clone(), message.clone()));
            error(&format!("Ошибка загрузки: {message}"));
            backup.event("ERROR", &format!("{}: ошибка загрузки: {message}", subscription.id))?;
            return Ok(());
        }
    };

    let baseline = match baseline_path(subscription) {
        Ok(path) => path,
        Err(err) => {
            state.failed.push((subscription.id.clone(), err.to_string()));
            error(&err.to_string());
            backup.event("ERROR", &format!("{}: {err}", subscription.id))?;
            return Ok(());
        }
    };
    let previous = match fs::read(&baseline) {
        Ok(bytes) => {
            backup.event("BASELINE", &format!("{}: baseline прочитан из {} ({} байт).", subscription.id, baseline.display(), bytes.len()))?;
            Some(bytes)
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            warning("Baseline отсутствует: подписка считается новой, сравнение пропускается.");
            backup.event("BASELINE", &format!("{}: baseline отсутствует; подписка рассматривается как новая.", subscription.id))?;
            None
        }
        Err(err) => {
            state.failed.push((subscription.id.clone(), err.to_string()));
            error(&format!("Не удалось прочитать baseline: {err}"));
            backup.event("ERROR", &format!("{}: не удалось прочитать baseline {}: {err}", subscription.id, baseline.display()))?;
            return Ok(());
        }
    };

    // Validate an existing baseline before the raw-byte comparison. A damaged
    // baseline must not be accepted merely because its bytes match the download.
    let mut damaged_baseline = false;
    let previous_parsed = match previous.as_deref() {
        Some(bytes) => match parse_subscription(bytes) {
            Ok(parsed) => Some(parsed),
            Err(err) => {
                warning(&format!(
                    "Baseline повреждён и будет рассматриваться как отсутствующий: {err}"
                ));
                backup.event("BASELINE", &format!("{}: baseline повреждён и будет считаться отсутствующим: {err}", subscription.id))?;
                damaged_baseline = true;
                None
            }
        },
        None => None,
    };

    if let Some(previous_bytes) = previous.as_deref().filter(|_| previous_parsed.is_some()) {
        if byte_contents_equal(Some(previous_bytes), &downloaded) {
            state.processed += 1;
            success("Уровень 1: исходный файл полностью совпадает с корректным baseline. Подписка не изменилась.");
            backup.event("COMPARE", &format!("{}: уровень 1 — исходные байты совпадают с baseline; изменений нет.", subscription.id))?;
            return Ok(());
        }
        info("Уровень 1: исходный файл отличается от baseline.");
        backup.event("COMPARE", &format!("{}: уровень 1 — исходные байты отличаются от baseline.", subscription.id))?;
    } else {
        info("Уровень 1: сравнение пропущено — baseline отсутствует или повреждён.");
        backup.event("COMPARE", &format!("{}: уровень 1 пропущен — baseline отсутствует или повреждён.", subscription.id))?;
    }

    let current = match parse_subscription(&downloaded) {
        Ok(parsed) => {
            backup.event("PARSE", &format!("{}: подписка разобрана; распознано подключений: {}, пропущено: {}.", subscription.id, parsed.outbounds.len(), parsed.unrecognized))?;
            parsed
        }
        Err(err) => {
            state.failed.push((subscription.id.clone(), err.to_string()));
            error(&format!("Ошибка обработки новой подписки: {err}. Baseline не обновляется."));
            backup.event("ERROR", &format!("{}: ошибка разбора новой подписки: {err}. Baseline не обновляется.", subscription.id))?;
            return Ok(());
        }
    };

    let previous_json = match previous_parsed.as_ref() {
        Some(parsed) => match serialize_outbounds(&parsed.outbounds) {
            Ok(bytes) => Some(bytes),
            Err(err) => {
                let message = format!("Не удалось сформировать Outbound из baseline: {err}");
                state.failed.push((subscription.id.clone(), message.clone()));
                error(&message);
                backup.event("ERROR", &format!("{}: {message}", subscription.id))?;
                return Ok(());
            }
        },
        None => None,
    };
    let current_json = match serialize_outbounds(&current.outbounds) {
        Ok(bytes) => bytes,
        Err(err) => {
            let message = format!("Не удалось сформировать Outbound из новой подписки: {err}");
            state.failed.push((subscription.id.clone(), message.clone()));
            error(&message);
            backup.event("ERROR", &format!("{}: {message}", subscription.id))?;
            return Ok(());
        }
    };

    if let Some(previous_json) = previous_json.as_deref() {
        if byte_contents_equal(Some(previous_json), &current_json) {
            state.processed += 1;
            success("Уровень 2: сформированные Outbound полностью совпадают. Рабочая конфигурация и baseline не меняются.");
            backup.event("COMPARE", &format!("{}: уровень 2 — сформированные Outbound совпадают побайтно.", subscription.id))?;
            return Ok(());
        }
        info("Уровень 2: сформированные Outbound отличаются. Запускаем сравнение по Tag.");
        backup.event("COMPARE", &format!("{}: уровень 2 — Outbound отличаются; запускается сравнение по Tag.", subscription.id))?;
    } else {
        info("Уровень 2: сравнение пропущено — baseline отсутствует или повреждён. Запускаем сравнение по Tag как для новой подписки.");
        backup.event("COMPARE", &format!("{}: уровень 2 пропущен; выполняется сравнение новой подписки по Tag.", subscription.id))?;
    }

    info("Уровень 3: сравнение подключений по Tag...");
    let mut report = match compare_subscription(previous_json.as_deref(), &current_json) {
        Ok(report) => report,
        Err(err) => {
            state.failed.push((subscription.id.clone(), err.to_string()));
            error(&format!("Ошибка сравнения подписки: {err}"));
            backup.event("ERROR", &format!("{}: ошибка сравнения подключений: {err}", subscription.id))?;
            return Ok(());
        }
    };
    report.unrecognized_current += current.unrecognized;
    if let Some(previous) = &previous_parsed {
        report.unrecognized_previous += previous.unrecognized;
    }
    print_comparison(&report);
    backup.event(
        "COMPARE",
        &format!("{}: добавлено {}, удалено {}, изменено {}, без изменений {}; нераспознано в новой версии {}, в baseline {}.",
            subscription.id, report.added.len(), report.removed.len(), report.changed.len(), report.unchanged,
            report.unrecognized_current, report.unrecognized_previous),
    )?;
    for item in &report.added {
        backup.event("COMPARE", &format!("{}: добавлено подключение {}.", subscription.id, item.tag))?;
    }
    for item in &report.removed {
        backup.event("COMPARE", &format!("{}: удалено подключение {}.", subscription.id, item.tag))?;
    }
    for item in &report.changed {
        backup.event("COMPARE", &format!("{}: изменено подключение {}.", subscription.id, item.tag))?;
        for change in &item.changes {
            backup.event("COMPARE", &format!("{}: {} — {} -> {}.", subscription.id, change.path,
                show_value(change.previous.as_ref()), show_value(change.current.as_ref())))?;
        }
    }

    if current.unrecognized > 0 {
        warning(&format!(
            "Частично распознано: {} подключений пропущено. Ошибки разбора:",
            current.unrecognized
        ));
        for detail in current.errors.iter().take(10) {
            warning(&format!("  {detail}"));
            backup.event("PARSE", &format!("{}: {detail}", subscription.id))?;
        }
        warning("Распознанные подключения будут обработаны. Старые подключения без однозначного соответствия не добавляются.");
    }

    let changed = !report.added.is_empty() || !report.removed.is_empty() || !report.changed.is_empty();
    if !changed {
        state.processed += 1;
        success("Изменений среди распознанных подключений нет. Рабочая группа и baseline сохраняются без изменений.");
        backup.event("SUBSCRIPTION", &format!("{}: среди распознанных подключений изменений нет; baseline не обновляется.", subscription.id))?;
        return Ok(());
    }

    if damaged_baseline {
        state.baseline_removals.push((subscription.id.clone(), baseline.clone()));
        backup.event("BASELINE", &format!("{}: повреждённый baseline добавлен в план удаления: {}.", subscription.id, baseline.display()))?;
    }

    state.baseline_updates.push(PreparedBaseline {
        id: subscription.id.clone(),
        original: downloaded,
        path: baseline.clone(),
    });
    backup.event("BASELINE", &format!("{}: новый baseline подготовлен для {}.", subscription.id, baseline.display()))?;

    let outbounds: Vec<Value> = current.outbounds.into_iter().map(|mut outbound| {
        let original_tag = outbound.get("tag").and_then(Value::as_str).unwrap_or("node");
        let tag = format!("{}{}", subscription.prefix, original_tag);
        if let Some(object) = outbound.as_object_mut() {
            object.insert("tag".to_owned(), Value::String(tag));
        }
        outbound
    }).collect();

    state.processed += 1;
    success(&format!("В Обновлённый Outbound передана вся группа ({} подключений).", outbounds.len()));
    backup.event("SUBSCRIPTION", &format!("{}: в Обновлённый Outbound передано {} подключений.", subscription.id, outbounds.len()))?;
    state.prepared.push(PreparedSubscription {
        id: subscription.id.clone(),
        outbounds,
    });
    Ok(())
}

fn baseline_path(subscription: &Subscription) -> AppResult<PathBuf> {
    let name = subscription
        .name
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("У подписки {} не задано имя для baseline.", subscription.id))?;

    for component in [&subscription.id, name] {
        if component == "." || component == ".."
            || component.contains('/')
            || component.contains('\\')
        {
            return Err(format!("Недопустимое имя baseline для {}.", subscription.id).into());
        }
    }

    Ok(Path::new(BASELINE_DIR).join(format!("{}--{}.json", subscription.id, name)))
}

fn byte_contents_equal(previous: Option<&[u8]>, current: &[u8]) -> bool {
    matches!(previous, Some(previous) if previous == current)
}

fn serialize_outbounds(outbounds: &[Value]) -> Result<Vec<u8>, serde_json::Error> {
    serde_json::to_vec(&serde_json::json!({ "outbounds": outbounds }))
}

fn load_config_documents() -> AppResult<Vec<ConfigDocument>> {
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

fn find_doc(docs: &[ConfigDocument], name: &str) -> Option<usize> {
    docs.iter().position(|doc| doc.file_name == name)
}

fn collect_api_tags(docs: &[ConfigDocument]) -> HashSet<String> {
    docs.iter().filter_map(|doc| doc.value.get("api"))
        .filter_map(|api| api.get("tag")).filter_map(Value::as_str)
        .map(str::to_owned).collect()
}

fn updated_groups(prepared: &[PreparedSubscription]) -> BTreeMap<u64, Vec<Value>> {
    prepared.iter().filter_map(|item| {
        subscription_id_number(&item.id).map(|n| (n, item.outbounds.clone()))
    }).collect()
}

fn validate_all(
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
                Err(format!("Xray config test завершился с ошибкой (код {code}):\\n{details}"))
            }
        }
        Err(other) => Err(format!("Xray config test: {other}")),
    }
}

fn print_comparison(report: &ComparisonReport) {
    for item in &report.added { success(&format!("  + Добавлено: {}", item.tag)); }
    for item in &report.removed { error(&format!("  - Удалено: {}", item.tag)); }
    for item in &report.changed {
        warning(&format!("  Изменено: {}", item.tag));
        for change in &item.changes {
            info(&format!("    {}: {} -> {}", change.path, show_value(change.previous.as_ref()), show_value(change.current.as_ref())));
        }
    }
    info(&format!("  Без изменений: {}", report.unchanged));
    if report.unrecognized_current > 0 { warning(&format!("  Нераспознанных в загрузке: {}", report.unrecognized_current)); }
    if report.unrecognized_previous > 0 { warning(&format!("  Нераспознанных в baseline: {}", report.unrecognized_previous)); }
    if report.identical_duplicates_removed > 0 { warning(&format!("  Устранено одинаковых дубликатов: {}", report.identical_duplicates_removed)); }
}

fn print_subscription_summary(state: &RunState) {
    println!();
    title("ОТЧЁТ ПО ПОДПИСКАМ");
    info(&format!("Успешно загружено: {}", state.downloaded));
    info(&format!("Успешно обработано: {}", state.processed));
    info(&format!("Подписок с изменениями: {}", state.prepared.len()));
    info(&format!("Ошибок: {}", state.failed.len()));
    for (id, message) in &state.failed { error(&format!("  {id}: {message}")); }
}

fn print_updated_outbound(prepared: &[PreparedSubscription]) {
    let outbounds: Vec<Value> = prepared
        .iter()
        .flat_map(|item| item.outbounds.iter().cloned())
        .collect();
    print_json_section("ОБНОВЛЁННЫЙ OUTBOUND", &Value::Array(outbounds));
}

fn print_json_section(label: &str, value: &Value) {
    let count = value.as_array().map(Vec::len).unwrap_or(0);
    info(&format!("{label}: {count} элементов подготовлено; содержимое скрыто."));
}

fn print_merge_summary(replacements: &[String], conflicts: &[String], archive: &ArchivePlan) {
    println!();
    title("ОБЪЕДИНЕНИЕ OUTBOUND");
    info("Изменившиеся группы заменяются целиком; остальные группы берутся из Рабочего Outbound.");
    info("Пользовательские подключения располагаются в конце.");
    for item in replacements { success(item); }
    for item in conflicts { warning(item); }
    info(&format!("Будет добавлено в архив: {}. Путь: {}.", archive.added, USER_ARCHIVE));
    if archive.added > 0 {
        info(&format!("В архив подготовлено {} подключений; содержимое скрыто.", archive.added));
    }
}

fn print_configuration_summary(
    docs: &[ConfigDocument], outbound_index: usize, routing_index: usize,
    obs: &[ObservatoryUpdate], balancer: &BalancerPreparation, tags: &[String],
) {
    println!();
    title("ПОДГОТОВКА КОНФИГУРАЦИИ");
    info(&format!("Итоговый Outbound: {} подключений.", tags.len()));
    info(&format!("Outbound: {}", docs[outbound_index].path.display()));
    info(&format!("Routing: {}", docs[routing_index].path.display()));
    for item in obs { success(&format!("subjectSelector обновлён: {} ({})", item.file_name, item.object_name)); }
    if balancer.user_balancer_tags.is_empty() {
        info("Пользовательские балансировщики не обнаружены.");
    } else {
        info(&format!(
            "Обнаружены пользовательские балансировщики ({}): {}.",
            balancer.user_balancer_tags.len(),
            balancer.user_balancer_tags.join(", ")
        ));
    }
    match balancer.case {
        BalancerCase::NoExistingBalancers => info("Существующих балансировщиков нет."),
        BalancerCase::OneExistingBalancer => info("Обнаружен один существующий балансировщик."),
        BalancerCase::MultipleExistingBalancers => info("Обнаружено несколько существующих балансировщиков."),
    }
    if balancer.created { success(&format!("Подготовлено создание {}.", balancer.tag)); }
    if balancer.updated_existing { success(&format!("Обновлён selector балансировщика {}.", balancer.tag)); }
    if balancer.transformed_rules > 0 { info(&format!("Routing rules перенаправлено: {}.", balancer.transformed_rules)); }
}

fn print_xray_test(result: &Result<(), String>) {
    println!();
    title("ПРОВЕРКА XRAY");
    match result {
        Ok(()) => success("Итоговая проверка Xray завершилась успешно."),
        Err(message) => {
            error(&format!("Xray отклонил итоговую конфигурацию: {message}"));
        }
    }
}

fn print_baseline_plan(state: &RunState) {
    println!();
    title("ПЛАН ОБНОВЛЕНИЯ BASELINE");
    if state.baseline_updates.is_empty() && state.baseline_removals.is_empty() {
        info("Изменений baseline не требуется.");
    }
    for (id, path) in &state.baseline_removals {
        warning(&format!("{id}: повреждённый baseline подготовлен к удалению после успешного применения изменений — {}.", path.display()));
    }
    for item in &state.baseline_updates {
        info(&format!(
            "{} -> {} ({} байт; сохранить после успешной замены рабочих файлов).",
            item.id, item.path.display(), item.original.len()
        ));
    }
    info("Baseline будет применён после конфигураций Xray и пользовательского архива.");
}

fn print_prepared_documents(docs: &[ConfigDocument]) {
    println!();
    title("ПОДГОТОВЛЕННЫЕ ДОКУМЕНТЫ (В ПАМЯТИ)");
    for doc in docs {
        let text = serde_json::to_string(&doc.value).unwrap_or_else(|_| doc.original.clone());
        info(&format!("{} — {} байт подготовлено.", doc.file_name, text.len()));
    }
}

fn build_file_plan(
    docs: &[ConfigDocument],
    state: &RunState,
    archive: &ArchivePlan,
) -> AppResult<Vec<PlannedFileOperation>> {
    let mut plan: BTreeMap<PathBuf, PlannedFileOperation> = BTreeMap::new();

    for doc in docs {
        // Keep untouched JSON/JSON5 files byte-for-byte as they were read.
        // Re-serialize only documents whose parsed configuration was changed.
        if doc.value != doc.original_value {
            let mut content = serde_json::to_vec_pretty(&doc.value)?;
            content.push(b'\n');
            plan.insert(doc.path.clone(), PlannedFileOperation {
                path: doc.path.clone(),
                category: "Конфигурация Xray",
                kind: PlannedOperationKind::Replace,
                content: Some(content),
            });
        }
    }

    if archive.added > 0 {
        let path = PathBuf::from(USER_ARCHIVE);
        let mut content = serde_json::to_vec_pretty(&archive.outbounds)?;
        content.push(b'\n');
        match fs::read(&path) {
            Ok(original) if original == content => {}
            Ok(_) => {
                plan.insert(path.clone(), PlannedFileOperation {
                    path,
                    category: "Архив пользовательских подключений",
                    kind: PlannedOperationKind::Replace,
                    content: Some(content),
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                plan.insert(path.clone(), PlannedFileOperation {
                    path,
                    category: "Архив пользовательских подключений",
                    kind: PlannedOperationKind::Create,
                    content: Some(content),
                });
            }
            Err(error) => return Err(format!("Не удалось проверить архив {}: {error}", path.display()).into()),
        }
    }

    for baseline in &state.baseline_updates {
        match fs::read(&baseline.path) {
            Ok(original) if original == baseline.original => {}
            Ok(_) => {
                plan.insert(baseline.path.clone(), PlannedFileOperation {
                    path: baseline.path.clone(),
                    category: "Baseline",
                    kind: PlannedOperationKind::Replace,
                    content: Some(baseline.original.clone()),
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                plan.insert(baseline.path.clone(), PlannedFileOperation {
                    path: baseline.path.clone(),
                    category: "Baseline",
                    kind: PlannedOperationKind::Create,
                    content: Some(baseline.original.clone()),
                });
            }
            Err(error) => return Err(format!("Не удалось проверить baseline {}: {error}", baseline.path.display()).into()),
        }
    }

    for (id, path) in &state.baseline_removals {
        if plan.contains_key(path) {
            // A replacement at the same path supersedes a separate removal.
            continue;
        }
        match fs::metadata(path) {
            Ok(metadata) if metadata.is_file() => {
                plan.insert(path.clone(), PlannedFileOperation {
                    path: path.clone(),
                    category: "Повреждённый baseline",
                    kind: PlannedOperationKind::Delete,
                    content: None,
                });
            }
            Ok(_) => return Err(format!("Путь baseline для {id} не является обычным файлом: {}", path.display()).into()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("Не удалось проверить baseline для удаления {}: {error}", path.display()).into()),
        }
    }

    Ok(plan.into_values().collect())
}

fn print_file_plan(plan: &[PlannedFileOperation]) {
    println!();
    title("ИТОГОВЫЙ СПИСОК ФАЙЛОВ ДЛЯ ИЗМЕНЕНИЯ");
    if plan.is_empty() {
        info("Файлы для изменения отсутствуют.");
        return;
    }
    for operation in plan {
        let action = match operation.kind {
            PlannedOperationKind::Create => "Создание",
            PlannedOperationKind::Replace => "Замена",
            PlannedOperationKind::Delete => "Удаление",
        };
        let size = operation.content.as_ref().map(|bytes| format!(" ({} байт)", bytes.len())).unwrap_or_default();
        info(&format!("{action}: {} — {}{size}", operation.path.display(), operation.category));
    }
    info(&format!("Всего операций: {}.", plan.len()));
}


#[cfg(test)]
mod file_plan_tests {
    use super::{build_file_plan, ConfigDocument, PlannedOperationKind, PreparedBaseline, RunState};
    use crate::update_merge::ArchivePlan;
    use serde_json::json;
    use std::fs;
    use std::path::PathBuf;

    fn temporary_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("xkeen-sub-{label}-{}", std::process::id()))
    }

    #[test]
    fn byte_identical_prepared_config_and_baseline_are_excluded() {
        let root = temporary_path("plan-identical");
        fs::create_dir_all(&root).unwrap();
        let baseline_path = root.join("baseline.json");
        let baseline = b"{\"subscriptions\":[]}".to_vec();
        fs::write(&baseline_path, &baseline).unwrap();

        let value = json!({"log":{"loglevel":"warning"}});
        let mut original = serde_json::to_vec_pretty(&value).unwrap();
        original.push(b'\n');
        let doc = ConfigDocument {
            file_name: "config.json".to_owned(),
            path: root.join("config.json"),
            original: String::from_utf8(original).unwrap(),
            original_value: value.clone(),
            value,
        };
        let state = RunState {
            baseline_updates: vec![PreparedBaseline {
                id: "sub1".to_owned(),
                original: baseline,
                path: baseline_path,
            }],
            ..RunState::default()
        };
        let archive = ArchivePlan { outbounds: Vec::new(), added: 0 };

        let plan = build_file_plan(&[doc], &state, &archive).unwrap();
        assert!(plan.is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unchanged_parsed_config_is_not_rewritten_when_source_format_differs() {
        let root = temporary_path("plan-semantic-equality");
        fs::create_dir_all(&root).unwrap();

        let original = "{\n  // Keep this comment and formatting.\n  \"log\": { \"loglevel\": \"warning\" }\n}\n";
        let value = serde_json::from_str::<serde_json::Value>(
            r#"{ "log": { "loglevel": "warning" } }"#,
        ).unwrap();
        let doc = ConfigDocument {
            file_name: "01_log.json".to_owned(),
            path: root.join("01_log.json"),
            original: original.to_owned(),
            original_value: value.clone(),
            value,
        };
        let state = RunState::default();
        let archive = ArchivePlan { outbounds: Vec::new(), added: 0 };

        let plan = build_file_plan(&[doc], &state, &archive).unwrap();
        assert!(plan.is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn changed_parsed_config_is_included_in_file_plan() {
        let root = temporary_path("plan-semantic-change");
        fs::create_dir_all(&root).unwrap();

        let original_value = json!({"log":{"loglevel":"warning"}});
        let current_value = json!({"log":{"loglevel":"debug"}});
        let doc = ConfigDocument {
            file_name: "01_log.json".to_owned(),
            path: root.join("01_log.json"),
            original: r#"{ "log": { "loglevel": "warning" } }"#.to_owned(),
            original_value,
            value: current_value,
        };
        let state = RunState::default();
        let archive = ArchivePlan { outbounds: Vec::new(), added: 0 };

        let plan = build_file_plan(&[doc], &state, &archive).unwrap();
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].kind, PlannedOperationKind::Replace);
        assert_eq!(plan[0].path, root.join("01_log.json"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn baseline_replacement_supersedes_removal_at_the_same_path() {
        let root = temporary_path("plan-baseline-priority");
        fs::create_dir_all(&root).unwrap();
        let baseline_path = root.join("baseline.json");
        fs::write(&baseline_path, b"old baseline").unwrap();

        let state = RunState {
            baseline_updates: vec![PreparedBaseline {
                id: "sub1".to_owned(),
                original: b"new baseline".to_vec(),
                path: baseline_path.clone(),
            }],
            baseline_removals: vec![("sub1".to_owned(), baseline_path.clone())],
            ..RunState::default()
        };
        let archive = ArchivePlan { outbounds: Vec::new(), added: 0 };

        let plan = build_file_plan(&[], &state, &archive).unwrap();
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].path, baseline_path);
        assert_eq!(plan[0].kind, PlannedOperationKind::Replace);
        fs::remove_dir_all(root).unwrap();
    }
}

fn show_value(value: Option<&Value>) -> String {
    value.map(|v| serde_json::to_string(v).unwrap_or_else(|_| v.to_string()))
        .unwrap_or_else(|| "<отсутствует>".to_owned())
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

fn command_exists(command: &str) -> bool {
    Command::new("sh").args(["-c", &format!("command -v {command} >/dev/null 2>&1")])
        .status().map(|s| s.success()).unwrap_or(false)
}

