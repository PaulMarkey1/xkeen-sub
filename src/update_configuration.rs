use std::collections::BTreeMap;
use std::error::Error;
use std::path::Path;

use serde_json::Value;

use crate::backup::{BackupSession, BackupSource};
use crate::blacklist::BlacklistStore;
use crate::balancer::{prepare_balancers, BalancerCase, BalancerPreparation, PROGRAM_BALANCER_TAG};
use crate::observatory::prepare_observatory_with_interval;
use crate::subscriptions::{Subscription, SubscriptionStore};
use crate::terminal::{error, info, success, title, warning, ProgressBar};
use crate::update_state::{PreparedSubscription, RunState};
use crate::update_apply::{self, PlannedOperationKind};
use crate::update_config::{collect_api_tags, find_doc, load_config_documents, validate_all, ConfigDocument, CONFIG_DIR, OUTBOUND_FILE, ROUTING_FILE};
use crate::update_merge::{collect_selector_tags, collect_tags, ensure_service_outbounds, load_archive, merge_outbounds, rebuild_outbounds, subscription_id_number, ArchivePlan};
use crate::update_plan::{build_file_plan, build_file_plan_with_blacklist, build_file_plan_with_registry, build_file_plan_with_registry_and_blacklist};
use crate::update_report::{print_baseline_plan, print_configuration_summary, print_file_plan, print_json_section, print_merge_summary, print_prepared_documents, print_subscription_summary, print_updated_outbound, print_xray_test};

type AppResult<T> = Result<T, Box<dyn Error>>;

pub(crate) struct PreparedConfiguration {
    pub(crate) docs: Vec<ConfigDocument>,
    balancer: BalancerPreparation,
    archive: ArchivePlan,
    pub(crate) validation: Result<(), String>,
}

pub(crate) fn update_configuration(
    subscriptions: &[Subscription],
    state: &RunState,
    backup: &mut BackupSession,
) -> AppResult<()> {
    update_configuration_with_mode(subscriptions, state, backup, false, None, None).map(|_| ())
}

pub(crate) fn update_configuration_with_blacklist(
    subscriptions: &[Subscription],
    state: &RunState,
    backup: &mut BackupSession,
    blacklist: &BlacklistStore,
) -> AppResult<()> {
    update_configuration_with_mode(subscriptions, state, backup, false, None, Some(blacklist)).map(|_| ())
}

/// Rebuilds every subscription-managed outbound group from baseline snapshots.
pub(crate) fn rebuild_configuration(
    subscriptions: &[Subscription],
    state: &RunState,
    backup: &mut BackupSession,
    store: &SubscriptionStore,
) -> AppResult<bool> {
    update_configuration_with_mode(subscriptions, state, backup, true, Some(store), None)
}

/// Rebuilds subscription groups and atomically persists a proposed blacklist.
pub(crate) fn rebuild_configuration_with_blacklist(
    subscriptions: &[Subscription],
    state: &RunState,
    backup: &mut BackupSession,
    store: &SubscriptionStore,
    blacklist: &BlacklistStore,
) -> AppResult<bool> {
    update_configuration_with_mode(subscriptions, state, backup, true, Some(store), Some(blacklist))
}

fn update_configuration_with_mode(
    subscriptions: &[Subscription],
    state: &RunState,
    backup: &mut BackupSession,
    rebuild_all: bool,
    registry: Option<&SubscriptionStore>,
    blacklist_override: Option<&BlacklistStore>,
) -> AppResult<bool> {
    let prepared = prepare_configuration(subscriptions, state, backup, rebuild_all, blacklist_override)?;
    apply_configuration(prepared, state, backup, registry, blacklist_override)
}

/// Builds the same in-memory configuration as update, but deliberately
/// returns it before planning or applying any filesystem operations.
pub(crate) fn prepare_configuration_for_test(
    subscriptions: &[Subscription],
    state: &RunState,
    backup: &mut BackupSession,
    blacklist: &BlacklistStore,
) -> AppResult<PreparedConfiguration> {
    prepare_configuration(subscriptions, state, backup, false, Some(blacklist))
}

fn prepare_configuration(
    subscriptions: &[Subscription],
    state: &RunState,
    backup: &mut BackupSession,
    rebuild_all: bool,
    blacklist_override: Option<&BlacklistStore>,
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
    let (mut final_outbounds, replacements, conflicts) = if rebuild_all {
        (rebuild_outbounds(&working, subscriptions, &updated_groups)?, Vec::new(), Vec::new())
    } else {
        merge_outbounds(&working, subscriptions, &updated_groups, &mut archive)?
    };
    let blacklist = match blacklist_override {
        Some(store) => store.clone(),
        None => crate::blacklist::load()?,
    };
    let filtered = blacklist.filter_outbounds(subscriptions, &mut final_outbounds)?;
    if filtered > 0 {
        backup.event("BLACKLIST", &format!("Исключено из итогового Outbound подключений по чёрному списку: {filtered}."))?;
    }
    let added_services = ensure_service_outbounds(&mut final_outbounds)?;
    for item in &added_services {
        backup.event("OUTBOUND", &format!("Добавлено обязательное системное подключение {item}."))?;
    }
    let all_tags = collect_tags(&final_outbounds)?;
    let tags = collect_selector_tags(&final_outbounds, &api_tags)?;
    print_json_section("ИТОГОВЫЙ OUTBOUND", &Value::Array(final_outbounds.clone()));

    let mut outbound_doc = docs[outbound_index].value.clone();
    outbound_doc.as_object_mut()
        .ok_or("Корень Outbound должен быть JSON-объектом.")?
        .insert("outbounds".to_owned(), Value::Array(final_outbounds.clone()));
    docs[outbound_index].value = outbound_doc;

    let balancing_mode = crate::balancer::detect_balancing_mode(&docs[routing_index].value);
    match balancing_mode {
        crate::balancer::BalancingMode::NotConfigured => {
            info("Режим балансировки: не настроен. Будет создан простой балансировщик xkeen-sub.");
            backup.event("BALANCER", "Режим балансировки не настроен; будет создан простой режим.")?;
        }
        crate::balancer::BalancingMode::Simple => {
            info("Режим балансировки: простой.");
            backup.event("BALANCER", "Обнаружен простой режим балансировки.")?;
        }
        crate::balancer::BalancingMode::Advanced => {
            info("Режим балансировки: продвинутый.");
            backup.event("BALANCER", "Обнаружен продвинутый режим балансировки.")?;
        }
        crate::balancer::BalancingMode::Custom => {
            info("Режим балансировки: пользовательский. Собственные балансировщики и правила маршрутизации останутся без изменений.");
            backup.event("BALANCER", "Обнаружен пользовательский режим; пользовательские балансировщики и правила не изменяются.")?;
        }
        crate::balancer::BalancingMode::IncompleteAdvanced => {
            warning("Обнаружен неполный продвинутый режим балансировки. Дополнительные балансировщики не будут автоматически восстанавливаться; обновится только основной балансировщик xkeen-sub.");
            backup.event("BALANCER", "Обнаружен неполный продвинутый режим; автоматическое восстановление отключено.")?;
        }
        crate::balancer::BalancingMode::Ambiguous => {
            warning("Не удалось однозначно определить режим балансировки: обнаружены повторяющиеся или противоречивые служебные балансировщики.");
            backup.event("BALANCER", "Режим балансировки неоднозначен; служебные структуры автоматически не исправляются.")?;
        }
    }


    let original_routing = docs[routing_index].value.clone();
    let balancer = prepare_balancers(
        &mut docs[routing_index].value, &working, &tags, &api_tags
    )?;
    let blocked_tags = blacklist.blocked_full_tags(subscriptions);

    let mut obs_updates = Vec::new();
    let mut observatory_tags = tags.clone();
    let advanced_mode = balancing_mode == crate::balancer::BalancingMode::Advanced;
    let mut advanced_selectors_applied = false;

    if advanced_mode {
        // Keep the three existing selectors intact until the helper Xray has
        // returned enough information to choose the new main/reserve nodes.
        docs[routing_index].value = original_routing;
        let initial_validation = validate_all(&docs, &final_outbounds, &all_tags, &balancer);
        if initial_validation.is_ok() {
            let freedom_tags: Vec<String> = final_outbounds.iter()
                .filter(|outbound| outbound.get("protocol").and_then(Value::as_str) == Some("freedom"))
                .filter_map(|outbound| outbound.get("tag").and_then(Value::as_str))
                .map(str::to_owned)
                .collect();
            if freedom_tags.is_empty() {
                return Err("В итоговом Outbound не найдено ни одного freedom-подключения для безопасного заполнения селекторов.".into());
            }

            let freedom_tag = freedom_tags.first()
                .cloned()
                .ok_or("В итоговом Outbound не найдено ни одного freedom-подключения для заполнения селекторов.")?;
            let probe_documents: Vec<(String, Value)> = docs.iter()
                .map(|doc| (doc.file_name.clone(), doc.value.clone()))
                .collect();
            let ranking = crate::speed_test::rank_outbounds_speed(&probe_documents, &tags)?;
            let main_tag = ranking.get(0).map(|item| item.0.clone()).unwrap_or_else(|| freedom_tag.clone());
            let reserve_tag = ranking.get(1).map(|item| item.0.clone()).unwrap_or_else(|| freedom_tag.clone());
            let rest_tag = ranking.get(2).map(|item| item.0.clone()).unwrap_or_else(|| freedom_tag.clone());

            let main = vec![main_tag.clone()];
            let reserve = vec![reserve_tag.clone()];
            let rest = vec![rest_tag.clone()];
            crate::balancer::apply_advanced_selectors(
                &mut docs[routing_index].value,
                &main,
                &reserve,
                &rest,
            )?;
            observatory_tags = vec![main_tag.clone(), reserve_tag.clone(), rest_tag.clone()];
            advanced_selectors_applied = true;
            backup.event("BALANCER", &format!(
                "Advanced по результатам speed test: основной {}; резервный {}; третий {}. Не занятые позиции заполнены freedom.",
                main_tag, reserve_tag, rest_tag
            ))?;
            }
        }
    } else {
        let filtered_selectors = crate::balancer::filter_blacklisted_selectors(
            &mut docs[routing_index].value, &tags, &blocked_tags
        )?;
        if filtered_selectors > 0 {
            backup.event("BLACKLIST", &format!("Исключено заблокированных тегов из селекторов xkeen-sub: {filtered_selectors}."))?;
        }
    }

    if !advanced_mode || advanced_selectors_applied {
        let mut obs_docs: Vec<(String, Value)> =
            docs.iter().map(|doc| (doc.file_name.clone(), doc.value.clone())).collect();
        let observatory_interval = if advanced_mode { Some("10s") } else { None };
        obs_updates =
            prepare_observatory_with_interval(&mut obs_docs, &observatory_tags, observatory_interval)?;
        for (name, value) in obs_docs {
            if let Some(index) = docs.iter().position(|doc| doc.file_name == name) {
                docs[index].value = value;
            } else {
                docs.push(ConfigDocument {
                    file_name: name.clone(),
                    path: Path::new(CONFIG_DIR).join(&name),
                    original: String::new(),
                    original_value: Value::Null,
                    value,
                    is_new: true,
                });
            }
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

    backup.event("CONFIG", "Все изменения балансировщиков и Observatory подготовлены. Запускается повторная проверка итоговой конфигурации Xray.")?;
    let validation = validate_all(&docs, &final_outbounds, &all_tags, &balancer);
    match &validation {
        Ok(()) => backup.event("XRAY", "Повторная проверка итоговой конфигурации после обновления балансировщиков и Observatory пройдена.")?,
        Err(message) => backup.event("XRAY", &format!("Повторная проверка итоговой конфигурации не пройдена. Рабочие файлы не изменялись: {message}"))?,
    }

    print_subscription_summary(&state);
    print_updated_outbound(&state.prepared);
    print_merge_summary(&replacements, &conflicts, &archive);
    print_configuration_summary(&docs, outbound_index, routing_index, &obs_updates, &balancer, &all_tags);
    print_xray_test(&validation);
    print_baseline_plan(&state);
    print_prepared_documents(&docs);


    Ok(PreparedConfiguration {
        docs,
        balancer,
        archive,
        validation,
    })
}

fn apply_configuration(
    prepared: PreparedConfiguration,
    state: &RunState,
    backup: &mut BackupSession,
    registry: Option<&SubscriptionStore>,
    blacklist_override: Option<&BlacklistStore>,
) -> AppResult<bool> {
    let PreparedConfiguration {
        docs,
        balancer,
        archive,
        validation,
    } = prepared;

    let mut no_file_changes = false;
    if let Err(message) = &validation {
        backup.event("XRAY", &format!("Проверка Xray не пройдена: {message}"))?;
        backup.set_outcome("ЗАБЛОКИРОВАНО", "Повторная проверка итоговой конфигурации Xray не пройдена. План применения не формировался; рабочие файлы балансировщиков и Observatory остались без изменений.", true);
    } else {
        backup.event("XRAY", "Проверка Xray завершилась успешно.")?;
        let plan = match (registry, blacklist_override) {
            (Some(store), Some(blacklist)) => build_file_plan_with_registry_and_blacklist(&docs, state, &archive, store, blacklist)?,
            (Some(store), None) => build_file_plan_with_registry(&docs, state, &archive, store)?,
            (None, Some(blacklist)) => build_file_plan_with_blacklist(&docs, state, &archive, blacklist)?,
            (None, None) => build_file_plan(&docs, state, &archive)?,
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
                backup.event("BACKUP", &format!("Проверка файлов по полному снимку завершена: {saved} файлов."))?;
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
                            true,
                        );
                        return Err(error);
                    }
                }
            }
            Err(error) => {
                progress.stop("Резервную копию не удалось создать");
                backup.event("ERROR", &format!("Резервирование не пройдено: {error}"))?;
                backup.set_outcome("ОШИБКА РЕЗЕРВИРОВАНИЯ", &format!("Применение заблокировано: {error}"), true);
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
    Ok(validation.is_ok())
}

fn updated_groups(prepared: &[PreparedSubscription]) -> BTreeMap<u64, Vec<Value>> {
    prepared.iter().filter_map(|item| {
        subscription_id_number(&item.id).map(|n| (n, item.outbounds.clone()))
    }).collect()
}

