use serde_json::Value;

use crate::balancer::{BalancerCase, BalancerPreparation};
use crate::observatory::ObservatoryUpdate;
use crate::outbound_compare::ComparisonReport;
use crate::update_state::RunState;
use crate::update_apply::{PlannedFileOperation, PlannedOperationKind};
use crate::update_config::ConfigDocument;
use crate::update_merge::ArchivePlan;
use crate::update_state::PreparedSubscription;

use crate::terminal::{error, info, info_prefix, success, title, warning};

pub(crate) fn print_comparison(report: &ComparisonReport) {
    for item in &report.added { success(&format!("  + Добавлено: {}", item.tag)); }
    for item in &report.removed { error(&format!("  - Удалено: {}", item.tag)); }
    for item in &report.changed {
        warning(&format!("  Изменено: {}", item.tag));
        for change in &item.changes {
            info_prefix(&change.path, &format!("значение изменилось: {} → {}", show_value(change.previous.as_ref()), show_value(change.current.as_ref())));
        }
    }
    info(&format!("  Без изменений: {}", report.unchanged));
    if report.unrecognized_current > 0 { warning(&format!("  Нераспознанных в загрузке: {}", report.unrecognized_current)); }
    if report.unrecognized_previous > 0 { warning(&format!("  Нераспознанных в baseline: {}", report.unrecognized_previous)); }
    if report.identical_duplicates_removed > 0 { warning(&format!("  Устранено одинаковых дубликатов: {}", report.identical_duplicates_removed)); }
}

pub(crate) fn print_subscription_summary(state: &RunState) {
    println!();
    title("ОТЧЁТ ПО ПОДПИСКАМ");
    info(&format!("Успешно загружено: {}", state.downloaded));
    info(&format!("Успешно обработано: {}", state.processed));
    info(&format!("Подписок с изменениями: {}", state.prepared.len()));
    info(&format!("Ошибок: {}", state.failed.len()));
    for (id, message) in &state.failed { error(&format!("  {id}: {message}")); }
}

pub(crate) fn print_updated_outbound(prepared: &[PreparedSubscription]) {
    let outbounds: Vec<Value> = prepared
        .iter()
        .flat_map(|item| item.outbounds.iter().cloned())
        .collect();
    print_json_section("ОБНОВЛЁННЫЙ OUTBOUND", &Value::Array(outbounds));
}

pub(crate) fn print_json_section(label: &str, value: &Value) {
    let count = value.as_array().map(Vec::len).unwrap_or(0);
    info(&format!("{label}: подготовлено элементов — {count}. Подробный список скрыт, чтобы не перегружать экран."));
}

pub(crate) fn print_merge_summary(replacements: &[String], conflicts: &[String], archive: &ArchivePlan) {
    println!();
    title("ОБЪЕДИНЕНИЕ OUTBOUND");
    info("Если подписка изменилась, её группа обновляется целиком. Остальные подключения сохраняются.");
    info("Ваши собственные подключения остаются в конце списка.");
    for item in replacements { success(item); }
    for item in conflicts { warning(item); }
    info(&format!("В архив будут добавлены ваши прежние подключения: {}.", archive.added));
    if archive.added > 0 {
        info(&format!("Подготовлена копия {} подключений. Подробный список сохранён в архиве.", archive.added));
    }
}

pub(crate) fn print_configuration_summary(
    docs: &[ConfigDocument], outbound_index: usize, routing_index: usize,
    obs: &[ObservatoryUpdate], balancer: &BalancerPreparation, tags: &[String],
) {
    println!();
    title("ПОДГОТОВКА КОНФИГУРАЦИИ");
    info(&format!("Итоговый Outbound: {} подключений.", tags.len()));
    info_prefix(&docs[outbound_index].file_name, &format!("файл с подключениями: {}", docs[outbound_index].path.display()));
    info_prefix(&docs[routing_index].file_name, &format!("файл с правилами маршрутизации: {}", docs[routing_index].path.display()));
    for item in obs {
        let action = if item.created {
            "подготовлен шаблон"
        } else {
            "обновлён список проверяемых подключений"
        };
        info_prefix(&item.file_name, &format!("{action} ({})", item.object_name));
    }
    if balancer.user_balancer_tags.is_empty() {
        info("В конфигурации не найдено настроенных вами балансировщиков.");
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
    if balancer.created { info_prefix(&balancer.tag, "подготовлен новый балансировщик программы"); }
    if balancer.updated_existing { info_prefix(&balancer.tag, "обновлён список подключений балансировщика"); }
    if balancer.transformed_rules > 0 { info(&format!("Правил маршрутизации обновлено: {}.", balancer.transformed_rules)); }
}

pub(crate) fn print_xray_test(result: &Result<(), String>) {
    println!();
    title("ПРОВЕРКА XRAY");
    match result {
        Ok(()) => success("Итоговая проверка Xray завершилась успешно."),
        Err(message) => {
            error(&format!("Xray не принял новую конфигурацию: {message}"));
        }
    }
}

pub(crate) fn print_baseline_plan(state: &RunState) {
    println!();
    title("ПЛАН ОБНОВЛЕНИЯ BASELINE");
    if state.baseline_updates.is_empty() && state.baseline_removals.is_empty() {
        info("Изменений baseline не требуется.");
    }
    for (id, path) in &state.baseline_removals {
        info_prefix(id, &format!("повреждённая сохранённая копия будет удалена после успешного обновления: {}.", path.display()));
    }
    for item in &state.baseline_updates {
        info_prefix(&item.id, &format!(
            "обновится после успешной обработки: {} ({} байт).",
            item.path.display(), item.original.len()
        ));
    }
    info("При изменении поддерживаемых подключений baseline применяется вместе с рабочими файлами; при отсутствии таких изменений — отдельно, без проверки Xray и перезапуска XKeen.");
}

pub(crate) fn print_prepared_documents(docs: &[ConfigDocument]) {
    println!();
    title("ФАЙЛЫ, ПОДГОТОВЛЕННЫЕ К ОБНОВЛЕНИЮ");
    for doc in docs {
        let text = serde_json::to_string(&doc.value).unwrap_or_else(|_| doc.original.clone());
        info_prefix(&doc.file_name, &format!("подготовлен, {} байт.", text.len()));
    }
}

pub(crate) fn print_file_plan(plan: &[PlannedFileOperation]) {
    println!();
    title("ЧТО ИЗМЕНИТСЯ НА РОУТЕРЕ");
    if plan.is_empty() {
        info("Изменения файлов не нужны.");
        return;
    }
    for operation in plan {
        let action = match operation.kind {
            PlannedOperationKind::Create => "Создание",
            PlannedOperationKind::Replace => "Замена",
            PlannedOperationKind::Delete => "Удаление",
        };
        let size = operation.content.as_ref().map(|bytes| format!(" ({} байт)", bytes.len())).unwrap_or_default();
        info_prefix(&operation.path.display().to_string(), &format!("будет выполнено: {action}; {}{size}.", operation.category));
    }
    info(&format!("Всего файловых операций: {}.", plan.len()));
}


pub(crate) fn show_value(value: Option<&Value>) -> String {
    value.map(|v| serde_json::to_string(v).unwrap_or_else(|_| v.to_string()))
        .unwrap_or_else(|| "<отсутствует>".to_owned())
}

