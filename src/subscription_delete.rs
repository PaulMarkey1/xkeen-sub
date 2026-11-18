use std::error::Error;
use std::fs;

use serde_json::Value;

use crate::backup::BackupSession;
use crate::subscription_identity::{number_from_id, number_from_tag, SubscriptionIdentity};
use crate::subscription_parser::parse_subscription;
use crate::subscriptions::SubscriptionStore;
use crate::terminal::{error, success};
use crate::update_configuration::rebuild_configuration;
use crate::update_state::{PreparedBaseline, PreparedSubscription, RunState};
use crate::update_subscription::baseline_path;
use crate::update_config::{find_doc, load_config_documents, OUTBOUND_FILE};
use crate::subscriptions::save_subscriptions;

type AppResult<T> = Result<T, Box<dyn Error>>;

/// Removes one subscription and applies the complete rebuilt state as one transaction.
pub(crate) fn delete_subscription(
    mut store: SubscriptionStore,
    index: usize,
) -> AppResult<()> {
    if index >= store.subscriptions.len() {
        return Err("Выбранная подписка больше не существует.".into());
    }

    let selected_id = store.subscriptions[index].id.clone();
    if !subscription_is_in_working_config(&selected_id)? {
        store.subscriptions.remove(index);
        save_subscriptions(&store)?;
        success(&format!(
            "Подписка {selected_id} удалена из sub.json. В рабочих конфигурациях её подключений не было; пересборка и перезапуск XKeen не требуются."
        ));
        return Ok(());
    }

    let mut backup = BackupSession::start("Удаление подписки")?;
    let original_ids: Vec<String> = store.subscriptions.iter().map(|item| item.id.clone()).collect();
    backup.set_subscriptions(original_ids.clone());

    let result = (|| -> AppResult<()> {
        let state = prepare_deletion(&mut store, index, &mut backup)?;
        backup.event(
            "DELETE",
            &format!(
                "Подготовлено удаление {}. Оставшихся подписок: {}. Все группы будут пересобраны из baseline.",
                original_ids[index],
                store.subscriptions.len()
            ),
        )?;

        let applied = rebuild_configuration(&store.subscriptions, &state, &mut backup, &store)?;
        if !applied {
            error("Удаление отменено: итоговая конфигурация не прошла проверку Xray. Исходные файлы сохранены.");
            return Err("Удаление отменено: проверка итоговой конфигурации Xray не пройдена.".into());
        }

        backup.event(
            "DELETE",
            "Реестр подписок, baseline и рабочие конфигурации применены; проверка и перезапуск XKeen завершены.",
        )?;
        success("Подписка удалена. Рабочие подключения пересобраны, Xray проверен, XKeen перезапущен.");
        Ok(())
    })();

    if let Err(ref failure) = result {
        if backup.is_running() {
            backup.set_outcome(
                "ОШИБКА УДАЛЕНИЯ",
                &format!("Удаление не завершено: {failure}"),
                false,
            );
        }
    }

    let log_result = backup.finish();
    if let Err(log_error) = log_result {
        return Err(format!("Не удалось завершить журнал удаления: {log_error}").into());
    }
    result
}

fn prepare_deletion(
    store: &mut SubscriptionStore,
    removed_index: usize,
    backup: &mut BackupSession,
) -> AppResult<RunState> {
    let original_ids: Vec<String> = store.subscriptions.iter().map(|item| item.id.clone()).collect();
    let original_paths = store.subscriptions.iter()
        .map(baseline_path)
        .collect::<AppResult<Vec<_>>>()?;
    let removed_id = store.subscriptions[removed_index].id.clone();
    let removed_path = original_paths[removed_index].clone();

    store.subscriptions.remove(removed_index);

    let mut state = RunState::default();
    state.baseline_removals.push((removed_id.clone(), removed_path));

    for (new_index, subscription) in store.subscriptions.iter_mut().enumerate() {
        let old_index = if new_index < removed_index { new_index } else { new_index + 1 };
        let old_path = &original_paths[old_index];
        let old_id = original_ids[old_index].clone();

        let raw_baseline = fs::read(old_path).map_err(|failure| {
            format!(
                "Нельзя удалить подписку безопасно: отсутствует baseline оставшейся подписки {} ({}): {failure}",
                subscription.id,
                old_path.display()
            )
        })?;
        let parsed = parse_subscription(&raw_baseline).map_err(|failure| {
            format!(
                "Нельзя пересобрать подписку {} из baseline {}: {failure}",
                subscription.id,
                old_path.display()
            )
        })?;

        let identity = SubscriptionIdentity::from_number(
            new_index + 1,
            subscription.name.as_deref(),
        );
        subscription.id = identity.id;
        subscription.prefix = identity.prefix;

        let new_path = baseline_path(subscription)?;
        if old_path != &new_path {
            state.baseline_removals.push((old_id, old_path.clone()));
        }
        state.baseline_updates.push(PreparedBaseline {
            id: subscription.id.clone(),
            original: raw_baseline,
            path: new_path.clone(),
        });

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

        backup.event(
            "DELETE",
            &format!(
                "Подписка {} перенесена в {}. Из baseline восстановлено подключений: {}.",
                subscription.id,
                new_path.display(),
                outbounds.len()
            ),
        )?;
        state.prepared.push(PreparedSubscription {
            id: subscription.id.clone(),
            outbounds,
        });
        state.processed += 1;
    }

    state.baseline_removals.sort_by(|left, right| left.1.cmp(&right.1));
    state.baseline_removals.dedup_by(|left, right| left.1 == right.1);

    backup.set_updated_subscriptions(
        state.prepared.iter().map(|item| item.id.clone()).collect()
    );
    backup.event(
        "DELETE",
        &format!(
            "Подготовлены новые идентификаторы и префиксы. Удаляемая подписка: {removed_id}; пересобираемых групп: {}.",
            state.prepared.len()
        ),
    )?;

    // Keep the empty case valid: deleting the last subscription leaves only user outbounds.
    Ok(state)
}


/// Checks whether the selected subscription currently owns any outbound in Xray's
/// working outbound document. Registry membership alone is not sufficient.
fn subscription_is_in_working_config(subscription_id: &str) -> AppResult<bool> {
    let number = number_from_id(subscription_id)
        .ok_or_else(|| format!("Некорректный ID подписки: {subscription_id}."))?;
    let docs = load_config_documents()?;
    let outbound_doc = find_doc(&docs, OUTBOUND_FILE)
        .ok_or_else(|| format!("Не найден файл {OUTBOUND_FILE}."))?;
    let outbounds = docs[outbound_doc].value.get("outbounds")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("В файле {OUTBOUND_FILE} отсутствует массив outbounds."))?;

    Ok(subscription_is_in_outbounds(outbounds, number))
}

#[cfg(test)]
mod participation_tests {
    use super::subscription_is_in_outbounds;
    use serde_json::json;

    #[test]
    fn detects_subscription_group_by_its_tag_number() {
        let outbounds = vec![
            json!({"tag":"sub2--Germany--node","protocol":"vless"}),
            json!({"tag":"sub20--node","protocol":"trojan"}),
        ];
        assert!(subscription_is_in_outbounds(&outbounds, 2));
        assert!(subscription_is_in_outbounds(&outbounds, 20));
        assert!(!subscription_is_in_outbounds(&outbounds, 3));
    }

    #[test]
    fn ignores_user_outbounds_and_malformed_subscription_tags() {
        let outbounds = vec![
            json!({"tag":"custom-sub2-node","protocol":"vless"}),
            json!({"tag":"sub2","protocol":"vless"}),
            json!({"tag":"sub2-node","protocol":"vless"}),
        ];
        assert!(!subscription_is_in_outbounds(&outbounds, 2));
    }
}

fn subscription_is_in_outbounds(outbounds: &[Value], number: u64) -> bool {
    outbounds.iter().any(|outbound| {
        outbound.get("tag")
            .and_then(Value::as_str)
            .and_then(number_from_tag) == Some(number)
    })
}
