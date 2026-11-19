use std::collections::HashSet;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::backup::{BackupSession, BackupSource};
use crate::subscription_identity::{number_from_id, number_from_tag, SubscriptionIdentity};
use crate::subscription_parser::parse_subscription;
use crate::subscriptions::SubscriptionStore;
use crate::terminal::{ProgressBar, success};
use crate::update_apply;
use crate::update_configuration::rebuild_configuration;
use crate::update_merge::{rebuild_outbounds, ArchivePlan};
use crate::update_plan::build_file_plan_with_registry;
use crate::update_state::{PreparedBaseline, PreparedSubscription, RunState};
use crate::update_subscription::baseline_path;
use crate::update_config::{find_doc, load_config_documents, OUTBOUND_FILE};

type AppResult<T> = Result<T, Box<dyn Error>>;

/// Renames a subscription without downloading it. Active groups are rebuilt from
/// their saved baselines and applied through the normal configuration pipeline.
pub(crate) fn rename_subscription(
    store: &mut SubscriptionStore,
    index: usize,
    requested_name: Option<String>,
) -> AppResult<()> {
    if index >= store.subscriptions.len() {
        return Err("Выбранная подписка больше не существует.".into());
    }

    let new_name = normalize_name(requested_name)?;
    let old_subscription = &store.subscriptions[index];
    let target_id = old_subscription.id.clone();
    let target_number = number_from_id(&target_id)
        .ok_or_else(|| format!("Некорректный ID подписки: {target_id}."))?;
    let target_number_usize = usize::try_from(target_number)
        .map_err(|_| format!("Номер подписки слишком велик: {target_id}."))?;
    let new_identity = SubscriptionIdentity::from_number(
        target_number_usize,
        new_name.as_deref(),
    );

    if old_subscription.name == new_name
        && old_subscription.prefix == new_identity.prefix
    {
        return Ok(());
    }

    let old_path = baseline_path(old_subscription)?;
    let mut proposed_store = store.clone();
    proposed_store.subscriptions[index].name = new_name.clone();
    proposed_store.subscriptions[index].prefix = new_identity.prefix.clone();
    validate_registry(&proposed_store)?;

    let new_path = baseline_path(&proposed_store.subscriptions[index])?;
    if old_path != new_path && path_exists(&new_path)? {
        return Err(format!(
            "Нельзя переименовать подписку: целевой baseline уже существует: {}",
            new_path.display()
        ).into());
    }

    let active_numbers = working_subscription_numbers()?;
    let registered_numbers: HashSet<u64> = proposed_store.subscriptions.iter()
        .filter_map(|subscription| number_from_id(&subscription.id))
        .collect();
    if let Some(orphan) = active_numbers.iter().find(|number| !registered_numbers.contains(number)) {
        return Err(format!(
            "В рабочем Outbound найдена группа sub{orphan}, которой нет в реестре подписок. Переименование остановлено, чтобы не удалить неизвестные подключения."
        ).into());
    }

    let target_is_active = active_numbers.contains(&target_number);
    let old_baseline_exists = regular_file_exists(&old_path)?;
    if target_is_active && !old_baseline_exists {
        return Err(format!(
            "Активная подписка {target_id} не может быть переименована: отсутствует baseline {}.",
            old_path.display()
        ).into());
    }

    let mut backup = BackupSession::start("Переименование подписки")?;
    backup.set_subscriptions(vec![target_id.clone()]);

    let operation = (|| -> AppResult<()> {
        let mut state = RunState::default();

        if target_is_active {
            let mut prepared_numbers = HashSet::new();

            // rebuild_outbounds() intentionally reconstructs every active
            // subscription group. Supplying only the renamed group would drop
            // all other subscription-managed groups.
            for subscription in &proposed_store.subscriptions {
                let number = number_from_id(&subscription.id)
                    .ok_or_else(|| format!("Некорректный ID подписки: {}.", subscription.id))?;
                if !active_numbers.contains(&number) {
                    continue;
                }
                prepared_numbers.insert(number);

                // The renamed subscription still has its baseline under the old
                // filename until the transaction applies the planned move.
                let path = if number == target_number {
                    old_path.clone()
                } else {
                    baseline_path(subscription)?
                };
                if !regular_file_exists(&path)? {
                    return Err(format!(
                        "Нельзя пересобрать активную подписку {}: отсутствует baseline {}.",
                        subscription.id,
                        path.display()
                    ).into());
                }
                if !backup.verify_original(&path)? {
                    return Err(format!(
                        "Baseline {} изменился после создания защитного снимка. Переименование остановлено.",
                        path.display()
                    ).into());
                }

                let raw_baseline = fs::read(&path)?;
                let outbounds = build_prefixed_outbounds(subscription, &raw_baseline)?;
                state.prepared.push(PreparedSubscription {
                    id: subscription.id.clone(),
                    outbounds,
                });

                if number == target_number && old_path != new_path {
                    state.baseline_updates.push(PreparedBaseline {
                        id: subscription.id.clone(),
                        original: raw_baseline,
                        path: new_path.clone(),
                    });
                    state.baseline_removals.push((target_id.clone(), old_path.clone()));
                }
            }

            if prepared_numbers != active_numbers {
                let missing = active_numbers.difference(&prepared_numbers)
                    .map(|number| format!("sub{number}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(format!(
                    "Не удалось подготовить baseline для всех активных групп: {missing}."
                ).into());
            }

            backup.set_updated_subscriptions(vec![target_id.clone()]);
            backup.event(
                "RENAME",
                &format!(
                    "Подготовлены baseline и Outbound всех активных подписок; изменяется имя {target_id}, ID остаётся прежним."
                ),
            )?;

            let applied = rebuild_configuration(
                &proposed_store.subscriptions,
                &state,
                &mut backup,
                &proposed_store,
            )?;
            if !applied {
                return Err("Переименование отменено: итоговая конфигурация Xray не прошла проверку.".into());
            }

            backup.event(
                "RENAME",
                "Рабочие Outbound, балансировщики, Observatory, baseline и реестр применены штатной процедурой пересборки; проверка Xray и запуск XKeen завершены.",
            )?;
        } else {
            // An inactive subscription has no working tags to rebuild. Move
            // its saved baseline (if present) and update the registry without
            // touching Xray configuration or restarting XKeen.
            if old_baseline_exists && old_path != new_path {
                if !backup.verify_original(&old_path)? {
                    return Err(format!(
                        "Baseline {} изменился после создания защитного снимка. Переименование остановлено.",
                        old_path.display()
                    ).into());
                }
                let raw_baseline = fs::read(&old_path)?;
                state.baseline_updates.push(PreparedBaseline {
                    id: target_id.clone(),
                    original: raw_baseline,
                    path: new_path.clone(),
                });
                state.baseline_removals.push((target_id.clone(), old_path.clone()));
            }

            let archive = ArchivePlan {
                outbounds: Vec::new(),
                added: 0,
            };
            let plan = build_file_plan_with_registry(
                &[],
                &state,
                &archive,
                &proposed_store,
            )?;
            backup.set_plan_count(plan.len());

            let sources: Vec<BackupSource> = plan.iter()
                .filter(|operation| operation.kind != update_apply::PlannedOperationKind::Create)
                .map(|operation| BackupSource {
                    path: operation.path.clone(),
                    category: operation.category.to_owned(),
                })
                .collect();
            let progress_total = if plan.is_empty() {
                0
            } else {
                sources.len() + plan.len() * 3 + 1
            };
            let mut progress = ProgressBar::new(progress_total);

            let saved = backup.backup_files_with_progress(&sources, &mut progress)?;
            backup.event(
                "BACKUP",
                &format!("Проверка файлов по полному снимку завершена: {saved} файлов."),
            )?;
            if let Err(error) = update_apply::execute_baseline_only(
                &plan,
                &mut backup,
                &mut progress,
            ) {
                progress.stop("Переименование остановлено");
                return Err(error);
            }
            progress.finish("Реестр и baseline обновлены");
            backup.set_outcome(
                "УСПЕШНО",
                "Переименована неактивная подписка. Обновлены только реестр и baseline; конфигурация Xray не менялась, XKeen не перезапускался.",
                true,
            );
        }

        Ok(())
    })();

    if let Err(ref error) = operation {
        if backup.is_running() {
            backup.set_outcome(
                "ОШИБКА ПЕРЕИМЕНОВАНИЯ",
                &format!("Переименование не завершено: {error}"),
                true,
            );
        }
    }

    let finish_result = backup.finish();
    if let Err(error) = finish_result {
        return Err(format!("Не удалось завершить журнал переименования: {error}").into());
    }

    operation?;
    *store = proposed_store;
    success(&format!("Подписка {target_id} переименована."));
    Ok(())
}

fn normalize_name(name: Option<String>) -> AppResult<Option<String>> {
    let name = name.map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    if let Some(value) = &name {
        if value == "." || value == ".."
            || value.contains('/')
            || value.contains('\\')
            || value.chars().any(char::is_control)
        {
            return Err(
                "Недопустимое имя подписки: нельзя использовать разделители пути и управляющие символы."
                    .into(),
            );
        }
    }
    Ok(name)
}

fn validate_registry(store: &SubscriptionStore) -> AppResult<()> {
    let mut ids = HashSet::new();
    let mut prefixes = HashSet::new();

    for subscription in &store.subscriptions {
        let number = number_from_id(&subscription.id)
            .ok_or_else(|| format!("Некорректный ID подписки: {}.", subscription.id))?;
        let number = usize::try_from(number)
            .map_err(|_| format!("Номер подписки слишком велик: {}.", subscription.id))?;
        let identity = SubscriptionIdentity::from_number(number, subscription.name.as_deref());

        if identity.id != subscription.id {
            return Err(format!("ID подписки не имеет канонический формат: {}.", subscription.id).into());
        }
        if subscription.prefix != identity.prefix {
            return Err(format!(
                "Префикс подписки {} не соответствует её имени. Сначала требуется исправить реестр.",
                subscription.id
            ).into());
        }
        if !ids.insert(subscription.id.clone()) {
            return Err(format!("В реестре повторяется ID {}.", subscription.id).into());
        }
        if !prefixes.insert(subscription.prefix.clone()) {
            return Err(format!("В реестре повторяется префикс {}.", subscription.prefix).into());
        }
        baseline_path(subscription)?;
    }

    Ok(())
}

fn path_exists(path: &Path) -> AppResult<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("Не удалось проверить путь {}: {error}", path.display()).into()),
    }
}

fn regular_file_exists(path: &Path) -> AppResult<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(format!("Путь baseline не является обычным файлом: {}.", path.display()).into())
        }
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("Не удалось проверить baseline {}: {error}", path.display()).into()),
    }
}

fn working_subscription_numbers() -> AppResult<HashSet<u64>> {
    let docs = load_config_documents()?;
    let outbound_index = find_doc(&docs, OUTBOUND_FILE)
        .ok_or_else(|| format!("Не найден файл {OUTBOUND_FILE}."))?;
    let outbounds = docs[outbound_index].value.get("outbounds")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("В файле {OUTBOUND_FILE} отсутствует массив outbounds."))?;

    Ok(outbounds.iter().filter_map(|outbound| {
        outbound.get("tag")
            .and_then(Value::as_str)
            .and_then(number_from_tag)
    }).collect())
}

fn build_prefixed_outbounds(
    subscription: &crate::subscriptions::Subscription,
    raw_baseline: &[u8],
) -> AppResult<Vec<Value>> {
    let parsed = parse_subscription(raw_baseline).map_err(|error| {
        format!(
            "Не удалось прочитать baseline подписки {}: {error}",
            subscription.id
        )
    })?;

    let mut outbounds = Vec::with_capacity(parsed.outbounds.len());
    for mut outbound in parsed.outbounds {
        let original_tag = outbound.get("tag").and_then(Value::as_str)
            .ok_or_else(|| format!("В baseline подписки {} найден Outbound без Tag.", subscription.id))?;
        let tag = format!("{}{}", subscription.prefix, original_tag);
        outbound.as_object_mut()
            .ok_or_else(|| format!("Outbound подписки {} должен быть JSON-объектом.", subscription.id))?
            .insert("tag".to_owned(), Value::String(tag));
        outbounds.push(outbound);
    }
    Ok(outbounds)
}

#[cfg(test)]
mod tests {
    use super::{build_prefixed_outbounds, normalize_name};
    use crate::subscriptions::Subscription;
    use serde_json::json;

    fn subscription(name: Option<&str>, prefix: &str) -> Subscription {
        Subscription {
            id: "sub2".to_owned(),
            name: name.map(str::to_owned),
            url: "https://example.test/sub".to_owned(),
            prefix: prefix.to_owned(),
        }
    }

    #[test]
    fn renaming_changes_only_the_outbound_tag_and_preserves_connection_data() {
        let item = subscription(Some("New"), "sub2--New--");
        let raw = br#"{"outbounds":[{"tag":"node","protocol":"vless","settings":{"id":"secret"}}]}"#;
        let outbounds = build_prefixed_outbounds(&item, raw).unwrap();

        assert_eq!(
            outbounds,
            vec![json!({
                "tag":"sub2--New--node",
                "protocol":"vless",
                "settings":{"id":"secret"}
            })]
        );
    }


    #[test]
    fn full_rebuild_preserves_other_active_subscriptions_and_user_outbounds() {
        use std::collections::BTreeMap;
        use crate::subscriptions::Subscription;

        let subscriptions = vec![
            Subscription {
                id: "sub1".to_owned(),
                name: Some("NewName".to_owned()),
                url: "https://example.test/one".to_owned(),
                prefix: "sub1--NewName--".to_owned(),
            },
            Subscription {
                id: "sub2".to_owned(),
                name: Some("Japan".to_owned()),
                url: "https://example.test/two".to_owned(),
                prefix: "sub2--Japan--".to_owned(),
            },
        ];
        let working = vec![
            json!({"tag":"sub1--OldName--node","protocol":"vless"}),
            json!({"tag":"sub2--Japan--keep","protocol":"trojan"}),
            json!({"tag":"my-user-node","protocol":"socks"}),
        ];
        let rebuilt = BTreeMap::from([
            (1, vec![json!({"tag":"sub1--NewName--node","protocol":"vless"})]),
            (2, vec![json!({"tag":"sub2--Japan--keep","protocol":"trojan"})]),
        ]);

        let result = rebuild_outbounds(&working, &subscriptions, &rebuilt).unwrap();
        let tags: Vec<&str> = result.iter()
            .map(|outbound| outbound["tag"].as_str().unwrap())
            .collect();

        assert_eq!(
            tags,
            vec![
                "sub1--NewName--node",
                "sub2--Japan--keep",
                "my-user-node",
            ]
        );
    }

    #[test]
    fn empty_name_is_normalized_to_none() {
        assert_eq!(normalize_name(Some("  ".to_owned())).unwrap(), None);
    }

    #[test]
    fn rejects_path_separators_and_control_characters_in_names() {
        assert!(normalize_name(Some("../outside".to_owned())).is_err());
        assert!(normalize_name(Some("bad\\name".to_owned())).is_err());
        assert!(normalize_name(Some("bad\nname".to_owned())).is_err());
    }
}
