use crate::balancer::{prepare_balancers, BalancerCase, BalancerPreparation, PROGRAM_BALANCER_TAG};
use crate::config::{self, ValidationMode};
use crate::observatory::{synchronize_subject_selectors, ObservatoryUpdate};
use crate::outbound_compare::{compare_subscription, extract_outbounds, ComparisonReport};
use crate::update_merge::{collect_tags, load_archive, merge_outbounds, subscription_id_number, ArchivePlan, USER_ARCHIVE};
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

type AppResult<T> = Result<T, Box<dyn Error>>;

const BASELINE_DIR: &str = "/opt/etc/xkeen-sub";
const CONFIG_DIR: &str = "/opt/etc/xray/configs";
const OUTBOUND_FILE: &str = "04_outbounds.json";
const ROUTING_FILE: &str = "05_routing.json";

struct ConfigDocument {
    file_name: String,
    path: PathBuf,
    original: String,
    value: Value,
}

struct PreparedSubscription {
    id: String,
    original: Vec<u8>,
    baseline: PathBuf,
    outbounds: Vec<Value>,
}

#[derive(Default)]
struct RunState {
    prepared: Vec<PreparedSubscription>,
    failed: Vec<(String, String)>,
    downloaded: usize,
    processed: usize,
}

/// Control run only: downloads and analyses subscriptions, constructs all
/// proposed documents in memory, validates them and prints the complete plan.
/// No active configuration, archive or baseline file is written.
pub fn run(target_id: Option<&str>) -> AppResult<()> {
    println!();
    title("XKEEN-SUB — КОНТРОЛЬНЫЙ ЗАПУСК UPDATE");
    warning("Рабочие файлы, архив и baseline не изменяются.");
    info("Резервное копирование и применение конфигурации будут реализованы отдельно.");

    let store = load_subscriptions()?;
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

    let mut docs = load_config_documents()?;
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

    let api_tags = collect_api_tags(&docs);
    let mut state = RunState::default();
    for subscription in selected {
        prepare_subscription(subscription, &mut state);
    }

    if state.downloaded == 0 {
        println!();
        info("Ни одна подписка не была успешно загружена. Проверка интернет-соединения...");
        if has_internet_connection() {
            warning("Интернет доступен, но серверы подписок не ответили.");
        } else {
            error("Отсутствует интернет-соединение.");
        }
        print_subscription_summary(&state);
        warning("Подготовка конфигурации прекращена. Рабочие файлы не изменены.");
        return Ok(());
    }
    if state.processed == 0 {
        print_subscription_summary(&state);
        warning("Ни одна подписка не прошла обработку. Подготовка конфигурации прекращена.");
        return Ok(());
    }

    let updated_groups = updated_groups(&state.prepared);
    let mut archive = load_archive()?;
    let (final_outbounds, replacements, conflicts) =
        merge_outbounds(&working, &store.subscriptions, &updated_groups, &mut archive)?;
    let tags = collect_tags(&final_outbounds)?;

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

    let mode = config::load()?.validation;
    let validation = validate_all(&docs, &final_outbounds, &tags, &balancer);

    print_subscription_summary(&state);
    print_merge_summary(&replacements, &conflicts, &archive);
    print_configuration_summary(&docs, outbound_index, routing_index, &obs_updates, &balancer, &tags);
    print_validation(mode, &validation);
    print_baseline_plan(&state.prepared);
    print_prepared_documents(&docs);

    println!();
    title("КОНТРОЛЬНЫЙ ЗАПУСК ЗАВЕРШЁН");
    warning("Ничего не применялось. Рабочие конфигурации, архив и baseline не изменены.");
    if balancer.created {
        warning(&format!(
            "После успешного применения будет выведено уведомление: балансировщик не был найден; создан балансировщик с Tag {}.",
            PROGRAM_BALANCER_TAG
        ));
        if balancer.case != BalancerCase::NoExistingBalancers {
            warning("Для дополнительного балансировщика routing rule потребуется настроить самостоятельно.");
        }
    }
    if validation.is_err() && mode == ValidationMode::On {
        error("Проверка блокирует будущее применение.");
    } else {
        success("План обновления подготовлен.");
    }
    Ok(())
}

fn prepare_subscription(subscription: &Subscription, state: &mut RunState) {
    println!();
    title(&format!("Подписка {}", subscription.id));
    info("Загрузка...");
    let downloaded = match download_subscription(subscription) {
        Ok(bytes) => {
            state.downloaded += 1;
            success(&format!("Получено {} байт.", bytes.len()));
            bytes
        }
        Err(err) => {
            state.failed.push((subscription.id.clone(), err.to_string()));
            error(&format!("Ошибка загрузки: {err}"));
            return;
        }
    };

    let baseline = match baseline_path(subscription) {
        Ok(path) => path,
        Err(err) => {
            state.failed.push((subscription.id.clone(), err.to_string()));
            error(&err.to_string());
            return;
        }
    };
    let previous = match fs::read(&baseline) {
        Ok(bytes) => Some(bytes),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            warning("Baseline отсутствует: подключения будут считаться новыми.");
            None
        }
        Err(err) => {
            state.failed.push((subscription.id.clone(), err.to_string()));
            error(&format!("Не удалось прочитать baseline: {err}"));
            return;
        }
    };

    info("Сравнение с baseline...");
    let report = match compare_subscription(previous.as_deref(), &downloaded) {
        Ok(report) => report,
        Err(err) => {
            state.failed.push((subscription.id.clone(), err.to_string()));
            error(&format!("Ошибка обработки подписки: {err}"));
            return;
        }
    };
    print_comparison(&report);

    if report.unrecognized_current > 0 {
        let message = format!(
            "В новой загрузке {} нераспознанных элементов. Безопасное сопоставление частично распознанной подписки ещё не реализовано.",
            report.unrecognized_current
        );
        state.failed.push((subscription.id.clone(), message.clone()));
        warning(&message);
        warning("Группа подписки оставлена из Рабочего Outbound, чтобы не потерять существующие подключения.");
        return;
    }

    let changed = !report.added.is_empty() || !report.removed.is_empty() || !report.changed.is_empty();
    if !changed {
        state.processed += 1;
        success("Изменений среди распознанных подключений нет. Рабочая группа сохранится.");
        if report.unrecognized_current > 0 {
            warning("Нераспознанные элементы новой загрузки не будут добавлены в Outbound.");
        }
        return;
    }

    let outbounds = match extract_outbounds(&downloaded) {
        Ok(items) => items.into_iter().map(|mut item| {
            let tag = format!("{}{}", subscription.prefix, item.tag);
            if let Some(object) = item.outbound.as_object_mut() {
                object.insert("tag".to_owned(), Value::String(tag));
            }
            item.outbound
        }).collect(),
        Err(err) => {
            state.failed.push((subscription.id.clone(), err.to_string()));
            error(&format!("Не удалось сформировать группу: {err}"));
            return;
        }
    };

    state.processed += 1;
    success(&format!("В Обновлённый Outbound передана вся группа ({} подключений).", outbounds.len()));
    state.prepared.push(PreparedSubscription {
        id: subscription.id.clone(),
        original: downloaded,
        baseline,
        outbounds,
    });
}

fn baseline_path(subscription: &Subscription) -> AppResult<PathBuf> {
    let named = subscription.name.as_deref().map(|s| !s.is_empty()).unwrap_or(false);
    let name = if named {
        subscription.prefix.strip_suffix("--").unwrap_or(&subscription.prefix).to_owned()
    } else {
        subscription.prefix.clone()
    };
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\\') {
        return Err(format!("Недопустимый префикс baseline для {}.", subscription.id).into());
    }
    Ok(Path::new(BASELINE_DIR).join(format!("{name}.json")))
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
        docs.push(ConfigDocument { file_name, path, original, value });
    }
    Ok(docs)
}

fn is_json_config(path: &Path) -> bool {
    matches!(path.extension().and_then(|e| e.to_str()), Some("json" | "jsonc"))
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

fn print_merge_summary(replacements: &[String], conflicts: &[String], archive: &ArchivePlan) {
    println!();
    title("ОБЪЕДИНЕНИЕ OUTBOUND");
    info("Изменившиеся группы заменяются целиком; остальные группы берутся из Рабочего Outbound.");
    info("Пользовательские подключения располагаются в конце.");
    for item in replacements { success(item); }
    for item in conflicts { warning(item); }
    info(&format!("Будет добавлено в архив: {}. Путь: {}.", archive.added, USER_ARCHIVE));
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
    match balancer.case {
        BalancerCase::NoExistingBalancers => warning("Существующие балансировщики не найдены."),
        BalancerCase::OneExistingBalancer => info("Найден один существующий балансировщик."),
        BalancerCase::MultipleExistingBalancers => info("Найдено несколько существующих балансировщиков."),
    }
    if balancer.created { success(&format!("Подготовлено создание {}.", balancer.tag)); }
    if balancer.updated_existing { success(&format!("Обновлён selector балансировщика {}.", balancer.tag)); }
    if balancer.transformed_rules > 0 { info(&format!("Routing rules перенаправлено: {}.", balancer.transformed_rules)); }
}

fn print_validation(mode: ValidationMode, result: &Result<(), String>) {
    println!();
    title("ВАЛИДАЦИЯ");
    info(&format!("validation: {}.", mode.as_str()));
    match result {
        Ok(()) => success("Обязательная итоговая проверка Xray завершилась успешно."),
        Err(message) if mode == ValidationMode::Alert => {
            warning(&format!("Xray отклонил итоговую конфигурацию: {message}"));
            error("Применять такую конфигурацию нельзя. Сначала необходимо устранить ошибку Xray.");
        }
        Err(message) => {
            error(&format!("Xray отклонил итоговую конфигурацию: {message}"));
            error("Применять такую конфигурацию нельзя.");
        }
    }
    if mode == ValidationMode::Off {
        info("Режим off отключает только предварительные проверки отдельных Outbound; итоговая проверка Xray обязательна.");
    }
}

fn print_baseline_plan(prepared: &[PreparedSubscription]) {
    println!();
    title("ПЛАН ОБНОВЛЕНИЯ BASELINE");
    if prepared.is_empty() { info("Baseline не требуется обновлять."); }
    for item in prepared {
        info(&format!("{} -> {} ({} байт).", item.id, item.baseline.display(), item.original.len()));
    }
    warning("В контрольном запуске baseline не записывается.");
}

fn print_prepared_documents(docs: &[ConfigDocument]) {
    println!();
    title("ПОДГОТОВЛЕННЫЕ ДОКУМЕНТЫ (В ПАМЯТИ)");
    for doc in docs {
        let text = serde_json::to_string_pretty(&doc.value).unwrap_or_else(|_| doc.original.clone());
        title(&format!("{} — {} байт", doc.file_name, text.len()));
        println!("{text}");
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

