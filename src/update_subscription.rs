use crate::backup::BackupSession;
use crate::outbound_compare::{compare_subscription, outbounds_differ_only_by_short_id};
use crate::subscription_download::download_subscription;
use crate::subscription_parser::parse_subscription;
use crate::subscriptions::Subscription;
use crate::terminal::{error, info, success, title, warning};
use crate::update_state::{DeferredShortIdUpdate, PreparedBaseline, PreparedSubscription, RunState, SubscriptionChanges};
use crate::update_report::{print_comparison, show_value};

use serde_json::Value;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

type AppResult<T> = Result<T, Box<dyn Error>>;

const BASELINE_DIR: &str = "/opt/etc/xkeen-sub/baseline";

fn record_observed_tags(
    state: &mut RunState,
    subscription: &Subscription,
    parsed: &crate::subscription_parser::ParsedSubscription,
) {
    if parsed.unrecognized > 0 || parsed.outbounds.is_empty() {
        return;
    }
    let tags: Vec<String> = parsed.outbounds.iter()
        .filter_map(|outbound| outbound.get("tag").and_then(Value::as_str).map(str::to_owned))
        .collect();
    if !tags.is_empty() && tags.len() == parsed.outbounds.len() {
        state.observed_tags.retain(|(id, _)| id != &subscription.id);
        state.observed_tags.push((subscription.id.clone(), tags));
    }
}

pub(crate) fn prepare_subscription(
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
            success("Сервер вернул те же данные, что и при прошлом обновлении. Изменений нет.");
            backup.event("COMPARE", &format!("{}: уровень 1 — исходные байты совпадают с baseline; изменений нет.", subscription.id))?;
            if let Some(parsed) = previous_parsed.as_ref() {
                record_observed_tags(state, subscription, parsed);
            }
            return Ok(());
        }
        info("Полученные данные отличаются от сохранённой копии. Проверяем, что именно изменилось.");
        backup.event("COMPARE", &format!("{}: уровень 1 — исходные байты отличаются от baseline.", subscription.id))?;
    } else {
        info("Сохранённой копии подписки нет или она повреждена. Сравним подключения по отдельности.");
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
            state.baseline_updates.push(PreparedBaseline {
                id: subscription.id.clone(),
                original: downloaded,
                path: baseline.clone(),
            });
            success("Состав поддерживаемых подключений не изменился. Будет обновлён только baseline.");
            backup.event("COMPARE", &format!(
                "{}: уровень 2 — сформированные Outbound совпадают побайтно; подготовлено обновление только baseline.",
                subscription.id
            ))?;
            record_observed_tags(state, subscription, &current);
            backup.event("BASELINE", &format!(
                "{}: исходные байты подписки изменились, но поддерживаемые Outbound прежние; новая версия подготовлена для baseline.",
                subscription.id
            ))?;
            return Ok(());
        }
        info("Состав подключений изменился. Теперь проверяем каждое подключение отдельно.");
        backup.event("COMPARE", &format!("{}: уровень 2 — Outbound отличаются; запускается сравнение по Tag.", subscription.id))?;
    } else {
        info("Нет сохранённой копии подписки. Проверяем подключения так же, как при первом добавлении.");
        backup.event("COMPARE", &format!("{}: уровень 2 пропущен; выполняется сравнение новой подписки по Tag.", subscription.id))?;
    }

    info("Сверяем подключения по их идентификаторам...");
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
    if !report.added.is_empty() || !report.removed.is_empty() || !report.changed.is_empty() {
        state.subscription_changes.push(SubscriptionChanges {
            id: subscription.id.clone(),
            added: report.added.iter().map(|item| item.tag.clone()).collect(),
            removed: report.removed.iter().map(|item| item.tag.clone()).collect(),
            changed: report.changed.iter().map(|item| item.tag.clone()).collect(),
        });
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

        if let Some(previous_json) = previous_json.as_deref() {
            if outbounds_differ_only_by_short_id(previous_json, &current_json)? {
                let outbounds = current.outbounds.iter().cloned().map(|mut outbound| {
                    let original_tag = outbound.get("tag").and_then(Value::as_str).unwrap_or("node");
                    let tag = format!("{}{}", subscription.prefix, original_tag);
                    if let Some(object) = outbound.as_object_mut() {
                        object.insert("tag".to_owned(), Value::String(tag));
                    }
                    outbound
                }).collect();

                state.deferred_short_id_updates.push(DeferredShortIdUpdate {
                    subscription: PreparedSubscription {
                        id: subscription.id.clone(),
                        outbounds,
                    },
                    baseline: PreparedBaseline {
                        id: subscription.id.clone(),
                        original: downloaded,
                        path: baseline.clone(),
                    },
                });
                success("Изменился только shortId. Обновление отложено до появления других изменений в подписках.");
                backup.event("COMPARE", &format!(
                    "{}: обнаружена только ротация shortId; подключение и baseline пока не обновляются.",
                    subscription.id
                ))?;
                record_observed_tags(state, subscription, &current);
                return Ok(());
            }
        }
        if previous.as_deref().map(|bytes| !byte_contents_equal(Some(bytes), &downloaded)).unwrap_or(true) {
            state.baseline_updates.push(PreparedBaseline {
                id: subscription.id.clone(),
                original: downloaded,
                path: baseline.clone(),
            });
            backup.event("BASELINE", &format!(
                "{}: поддерживаемые Outbound не изменились; новая исходная версия подписки подготовлена для обновления только baseline.",
                subscription.id
            ))?;
            success("Поддерживаемые подключения не изменились. Будет обновлён только baseline.");
        } else {
            success("Изменений среди распознанных подключений нет. Рабочая группа и baseline остаются без изменений.");
            backup.event("SUBSCRIPTION", &format!("{}: поддерживаемые подключения и исходные байты не изменились.", subscription.id))?;
        }
        record_observed_tags(state, subscription, &current);
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

    record_observed_tags(state, subscription, &current);
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

/// Prepares the selected subscriptions in order and records the resulting run state.
pub(crate) fn prepare_subscriptions(
    selected: &[&Subscription],
    backup: &mut BackupSession,
) -> AppResult<RunState> {
    let mut state = RunState::default();

    for subscription in selected {
        backup.event("SUBSCRIPTION", &format!("Начата обработка {}.", subscription.id))?;
        prepare_subscription(subscription, &mut state, backup)?;
        backup.event(
            "SUBSCRIPTION",
            &format!(
                "Обработка {} завершена. Загружено: {}, подготовлено изменений: {}, ошибок: {}.",
                subscription.id,
                state.downloaded,
                state.prepared.len(),
                state.failed.len()
            ),
        )?;
    }

    let promoted_short_id_updates = promote_deferred_short_id_updates(&mut state);
    for id in promoted_short_id_updates {
        backup.event("COMPARE", &format!(
            "{id}: отложенная ротация shortId применена вместе с другими изменениями подписок."
        ))?;
    }

    backup.set_updated_subscriptions(state.prepared.iter().map(|item| item.id.clone()).collect());
    Ok(state)
}

fn promote_deferred_short_id_updates(state: &mut RunState) -> Vec<String> {
    if state.prepared.is_empty() {
        return Vec::new();
    }

    let mut promoted = Vec::new();
    for deferred in state.deferred_short_id_updates.drain(..) {
        promoted.push(deferred.subscription.id.clone());
        state.prepared.push(deferred.subscription);
        state.baseline_updates.push(deferred.baseline);
    }
    promoted
}

pub(crate) fn baseline_path(subscription: &Subscription) -> AppResult<PathBuf> {
    let name = subscription.name.as_deref()
        .filter(|value| !value.trim().is_empty());

    if subscription.id == "." || subscription.id == ".."
        || subscription.id.contains('/')
        || subscription.id.contains('\\')
    {
        return Err(format!("Недопустимый ID для baseline: {}.", subscription.id).into());
    }
    if let Some(name) = name {
        if name == "." || name == ".." || name.contains('/') || name.contains('\\') {
            return Err(format!("Недопустимое имя baseline для {}.", subscription.id).into());
        }
    }

    let file_name = match name {
        Some(name) => format!("{}--{}.json", subscription.id, name),
        None => format!("{}.json", subscription.id),
    };
    Ok(Path::new(BASELINE_DIR).join(file_name))
}

fn byte_contents_equal(previous: Option<&[u8]>, current: &[u8]) -> bool {
    matches!(previous, Some(previous) if previous == current)
}

fn serialize_outbounds(outbounds: &[Value]) -> Result<Vec<u8>, serde_json::Error> {
    serde_json::to_vec(&serde_json::json!({ "outbounds": outbounds }))
}

#[cfg(test)]
mod subscription_comparison_tests {
    use super::{baseline_path, byte_contents_equal, serialize_outbounds};
    use crate::subscriptions::Subscription;
    use serde_json::json;
    use std::path::Path;

    #[test]
    fn short_id_only_updates_stay_deferred_when_no_other_subscription_changed() {
        use super::promote_deferred_short_id_updates;
        use crate::update_state::{DeferredShortIdUpdate, PreparedBaseline, PreparedSubscription, RunState};
        use serde_json::json;
        use std::path::PathBuf;

        let mut state = RunState::default();
        state.deferred_short_id_updates.push(DeferredShortIdUpdate {
            subscription: PreparedSubscription {
                id: "sub2".to_owned(),
                outbounds: vec![json!({"tag":"sub2--node","protocol":"vless"})],
            },
            baseline: PreparedBaseline {
                id: "sub2".to_owned(),
                original: b"new".to_vec(),
                path: PathBuf::from("/tmp/sub2.json"),
            },
        });

        assert!(promote_deferred_short_id_updates(&mut state).is_empty());
        assert!(state.prepared.is_empty());
        assert!(state.baseline_updates.is_empty());
        assert_eq!(state.deferred_short_id_updates.len(), 1);
    }

    #[test]
    fn short_id_only_updates_are_promoted_when_another_subscription_changed() {
        use super::promote_deferred_short_id_updates;
        use crate::update_state::{DeferredShortIdUpdate, PreparedBaseline, PreparedSubscription, RunState};
        use serde_json::json;
        use std::path::PathBuf;

        let mut state = RunState::default();
        state.prepared.push(PreparedSubscription {
            id: "sub1".to_owned(),
            outbounds: vec![json!({"tag":"sub1--node","protocol":"vless","port":8443})],
        });
        state.deferred_short_id_updates.push(DeferredShortIdUpdate {
            subscription: PreparedSubscription {
                id: "sub2".to_owned(),
                outbounds: vec![json!({"tag":"sub2--node","protocol":"vless"})],
            },
            baseline: PreparedBaseline {
                id: "sub2".to_owned(),
                original: b"new".to_vec(),
                path: PathBuf::from("/tmp/sub2.json"),
            },
        });

        assert_eq!(promote_deferred_short_id_updates(&mut state), vec!["sub2"]);
        assert_eq!(state.prepared.len(), 2);
        assert_eq!(state.baseline_updates.len(), 1);
        assert!(state.deferred_short_id_updates.is_empty());
    }

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
    fn baseline_path_supports_subscriptions_without_a_name() {
        let subscription = Subscription {
            id: "sub4".to_owned(),
            name: None,
            url: "https://example.test/sub".to_owned(),
            prefix: "sub4--".to_owned(),
        };

        assert_eq!(
            baseline_path(&subscription).unwrap(),
            Path::new("/opt/etc/xkeen-sub/baseline/sub4.json")
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

