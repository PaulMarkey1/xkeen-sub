use std::collections::BTreeMap;
use std::error::Error;

use serde_json::Value;

use crate::backup::{BackupSession, BackupSource};
use crate::balancer::{prepare_balancers, BalancerCase, BalancerPreparation, PROGRAM_BALANCER_TAG};
use crate::observatory::{synchronize_subject_selectors, ObservatoryUpdate};
use crate::subscriptions::{Subscription, SubscriptionStore};
use crate::terminal::{error, info, success, title, warning, ProgressBar};
use crate::update_state::{PreparedSubscription, RunState};
use crate::update_apply::{self, PlannedOperationKind};
use crate::update_config::{collect_api_tags, find_doc, load_config_documents, validate_all, ConfigDocument, OUTBOUND_FILE, ROUTING_FILE};
use crate::update_merge::{collect_selector_tags, collect_tags, load_archive, merge_outbounds, rebuild_outbounds, subscription_id_number, ArchivePlan};
use crate::update_plan::{build_file_plan, build_file_plan_with_registry};
use crate::update_report::{print_baseline_plan, print_configuration_summary, print_file_plan, print_json_section, print_merge_summary, print_prepared_documents, print_subscription_summary, print_updated_outbound, print_xray_test};

type AppResult<T> = Result<T, Box<dyn Error>>;

struct PreparedConfiguration {
    docs: Vec<ConfigDocument>,
    outbound_index: usize,
    routing_index: usize,
    obs_updates: Vec<ObservatoryUpdate>,
    balancer: BalancerPreparation,
    replacements: Vec<String>,
    conflicts: Vec<String>,
    archive: ArchivePlan,
    validation: Result<(), String>,
}

pub(crate) fn update_configuration(
    subscriptions: &[Subscription],
    state: &RunState,
    backup: &mut BackupSession,
) -> AppResult<()> {
    update_configuration_with_mode(subscriptions, state, backup, false, None)
}

/// Rebuilds every subscription-managed outbound group from baseline snapshots.
pub(crate) fn rebuild_configuration(
    subscriptions: &[Subscription],
    state: &RunState,
    backup: &mut BackupSession,
    store: &SubscriptionStore,
) -> AppResult<()> {
    update_configuration_with_mode(subscriptions, state, backup, true, Some(store))
}

fn update_configuration_with_mode(
    subscriptions: &[Subscription],
    state: &RunState,
    backup: &mut BackupSession,
    rebuild_all: bool,
    registry: Option<&SubscriptionStore>,
) -> AppResult<()> {
    let prepared = prepare_configuration(subscriptions, state, backup, rebuild_all)?;
    apply_configuration(prepared, state, backup, registry)
}

fn prepare_configuration(
    subscriptions: &[Subscription],
    state: &RunState,
    backup: &mut BackupSession,
    rebuild_all: bool,
) -> AppResult<PreparedConfiguration> {
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
    let (final_outbounds, replacements, conflicts) = if rebuild_all {
        (rebuild_outbounds(&working, subscriptions, &updated_groups)?, Vec::new(), Vec::new())
    } else {
        merge_outbounds(&working, subscriptions, &updated_groups, &mut archive)?
    };
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


    Ok(PreparedConfiguration {
        docs,
        outbound_index,
        routing_index,
        obs_updates,
        balancer,
        replacements,
        conflicts,
        archive,
        validation,
    })
}

fn apply_configuration(
    prepared: PreparedConfiguration,
    state: &RunState,
    backup: &mut BackupSession,
    registry: Option<&SubscriptionStore>,
) -> AppResult<()> {
    let PreparedConfiguration {
        docs,
        outbound_index,
        routing_index,
        obs_updates,
        balancer,
        replacements,
        conflicts,
        archive,
        validation,
    } = prepared;

    let mut no_file_changes = false;
    if let Err(message) = &validation {
        backup.event("XRAY", &format!("Проверка Xray не пройдена: {message}"))?;
        backup.set_outcome("ЗАБЛОКИРОВАНО", "Проверка Xray не пройдена. План применения не формировался, резервное копирование файлов не выполнялось.", false);
    } else {
        backup.event("XRAY", "Проверка Xray завершилась успешно.")?;
        let plan = match registry {
            Some(store) => build_file_plan_with_registry(&docs, state, &archive, store)?,
            None => build_file_plan(&docs, state, &archive)?,
        };
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

fn updated_groups(prepared: &[PreparedSubscription]) -> BTreeMap<u64, Vec<Value>> {
    prepared.iter().filter_map(|item| {
        subscription_id_number(&item.id).map(|n| (n, item.outbounds.clone()))
    }).collect()
}

