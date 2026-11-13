use crate::outbound_compare::outbound_parameters_equal;
use crate::subscriptions::Subscription;
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::error::Error;
use std::fs;
use std::path::Path;

type AppResult<T> = Result<T, Box<dyn Error>>;

const USER_ARCHIVE: &str = "/opt/etc/xkeen-sub/old-user-outbound.json";

pub(crate) struct ArchivePlan {
    pub(crate) outbounds: Vec<Value>,
    pub(crate) added: usize,
}


pub(crate) fn merge_outbounds(
    working: &[Value],
    subscriptions: &[Subscription],
    updated: &BTreeMap<u64, Vec<Value>>,
    archive: &mut ArchivePlan,
) -> AppResult<(Vec<Value>, Vec<String>, Vec<String>)> {
    let mut groups: BTreeMap<u64, Vec<Value>> = BTreeMap::new();
    let mut users = Vec::new();
    for outbound in working {
        let tag = outbound.get("tag").and_then(Value::as_str)
            .ok_or("В Рабочем Outbound найден элемент без Tag.")?;
        if let Some(number) = subscription_number(tag) {
            groups.entry(number).or_default().push(outbound.clone());
        } else {
            users.push(outbound.clone());
        }
    }
    for (number, group) in updated {
        groups.insert(*number, group.clone());
    }

    let mut replacements = Vec::new();
    let mut conflicts = Vec::new();
    let mut retained = Vec::new();

    for user in users {
        let user_tag = user.get("tag").and_then(Value::as_str).unwrap_or_default();
        let mut replaced = false;
        for subscription in subscriptions {
            let Some(number) = subscription_id_number(&subscription.id) else { continue };
            if !updated.contains_key(&number) { continue; }
            if let Some(group) = groups.get(&number) {
                for subscribed in group {
                    let subscribed_tag = subscribed.get("tag").and_then(Value::as_str).unwrap_or_default();
                    let original_tag = subscribed_tag.strip_prefix(&subscription.prefix).unwrap_or(subscribed_tag);
                    if original_tag != user_tag { continue; }
                    if outbound_parameters_equal(&user, subscribed) {
                        archive_outbound(archive, &user)?;
                        replacements.push(format!("{user_tag} заменено версией из {}", subscription.id));
                        replaced = true;
                        break;
                    } else {
                        conflicts.push(format!("{user_tag}: имя совпадает с {}, параметры различаются; пользовательская версия сохранена", subscription.id));
                    }
                }
            }
            if replaced { break; }
        }
        if !replaced { retained.push(user); }
    }

    let mut final_outbounds = Vec::new();
    for (_, group) in groups { final_outbounds.extend(group); }
    final_outbounds.extend(retained);
    Ok((final_outbounds, replacements, conflicts))
}

pub(crate) fn subscription_id_number(id: &str) -> Option<u64> {
    let number = id.strip_prefix("sub")?;
    if number.is_empty() || !number.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    number.parse().ok()
}

fn subscription_number(tag: &str) -> Option<u64> {
    let rest = tag.strip_prefix("sub")?;
    let (number, suffix) = rest.split_once("--")?;
    if suffix.is_empty() {
        return None;
    }
    subscription_id_number(&format!("sub{number}"))
}

pub(crate) fn load_archive() -> AppResult<ArchivePlan> {
    let path = Path::new(USER_ARCHIVE);
    if !path.exists() { return Ok(ArchivePlan { outbounds: Vec::new(), added: 0 }); }
    let text = fs::read_to_string(path)?;
    let value: Value = serde_json::from_str(&text)
        .map_err(|err| format!("Архив пользовательских Outbound повреждён: {err}"))?;
    let outbounds = value.as_array().cloned().or_else(|| value.get("outbounds").and_then(Value::as_array).cloned())
        .ok_or("В архиве отсутствует массив outbounds.")?;
    Ok(ArchivePlan { outbounds, added: 0 })
}

fn archive_outbound(archive: &mut ArchivePlan, outbound: &Value) -> AppResult<()> {
    let tag = outbound.get("tag").and_then(Value::as_str)
        .ok_or("Архивируемое подключение не содержит Tag.")?.to_owned();
    if let Some(existing) = archive.outbounds.iter().find(|v| v.get("tag").and_then(Value::as_str) == Some(tag.as_str())) {
        if outbound_parameters_equal(existing, outbound) { return Ok(()); }
    } else {
        archive.outbounds.push(outbound.clone());
        archive.added += 1;
        return Ok(());
    }
    let mut suffix = 1u64;
    loop {
        let candidate = format!("{tag}--{suffix}");
        match archive.outbounds.iter().find(|v| v.get("tag").and_then(Value::as_str) == Some(candidate.as_str())) {
            Some(existing) if outbound_parameters_equal(existing, outbound) => return Ok(()),
            Some(_) => suffix += 1,
            None => {
                let mut copy = outbound.clone();
                copy.as_object_mut().ok_or("Outbound архива должен быть объектом.")?
                    .insert("tag".to_owned(), Value::String(candidate));
                archive.outbounds.push(copy);
                archive.added += 1;
                return Ok(());
            }
        }
    }
}

pub(crate) fn collect_tags(outbounds: &[Value]) -> AppResult<Vec<String>> {
    let mut tags = Vec::new();
    let mut unique = HashSet::new();
    for outbound in outbounds {
        let tag = outbound.get("tag").and_then(Value::as_str)
            .filter(|tag| !tag.is_empty()).ok_or("Итоговый Outbound содержит пустой или отсутствующий Tag.")?;
        if !unique.insert(tag.to_owned()) {
            return Err(format!("В Итоговом Outbound повторяется Tag {tag}.").into());
        }
        tags.push(tag.to_owned());
    }
    Ok(tags)
}


#[cfg(test)]
mod tests {
    use super::{archive_outbound, subscription_id_number, subscription_number, ArchivePlan};
    use crate::outbound_compare::outbound_parameters_equal;
    use serde_json::json;

    #[test]
    fn identifies_subscription_tags() {
        assert_eq!(subscription_number("sub1--Germany--node"), Some(1));
        assert_eq!(subscription_id_number("sub20"), Some(20));
        assert_eq!(subscription_number("sub20--node"), Some(20));
        assert_eq!(subscription_number("custom"), None);
    }

    #[test]
    fn archive_deduplicates_and_adds_suffix_on_conflict() {
        let first = json!({"tag":"custom","protocol":"vless","settings":{"id":"one"}});
        let mut archive = ArchivePlan { outbounds: vec![first.clone()], added: 0 };
        archive_outbound(&mut archive, &first).unwrap();
        assert_eq!(archive.added, 0);

        let changed = json!({"tag":"custom","protocol":"vless","settings":{"id":"two"}});
        archive_outbound(&mut archive, &changed).unwrap();
        assert_eq!(archive.added, 1);
        assert_eq!(archive.outbounds[1]["tag"], "custom--1");
        assert!(!outbound_parameters_equal(&archive.outbounds[0], &archive.outbounds[1]));
    }

    #[test]
    fn replaces_only_changed_subscription_group_and_archives_exact_user_match() {
        use std::collections::BTreeMap;
        use crate::subscriptions::Subscription;
        use super::merge_outbounds;

        let subscription = Subscription {
            id: "sub1".to_owned(),
            name: Some("Germany".to_owned()),
            url: "https://example.com/sub".to_owned(),
            prefix: "sub1--Germany--".to_owned(),
        };
        let working = vec![
            json!({"tag":"sub1--Germany--old","protocol":"vless"}),
            json!({"tag":"sub2--Japan--keep","protocol":"vless"}),
            json!({"tag":"same-name","protocol":"vless","settings":{"id":"same"}}),
        ];
        let updated = BTreeMap::from([(
            1,
            vec![
                json!({"tag":"sub1--Germany--new","protocol":"vless"}),
                json!({"tag":"sub1--Germany--same-name","protocol":"vless","settings":{"id":"same"}}),
            ],
        )]);
        let mut archive = ArchivePlan { outbounds: Vec::new(), added: 0 };

        let (final_outbounds, replacements, conflicts) =
            merge_outbounds(&working, &[subscription], &updated, &mut archive).unwrap();

        let tags: Vec<&str> = final_outbounds.iter()
            .map(|item| item["tag"].as_str().unwrap()).collect();
        assert_eq!(tags, vec![
            "sub1--Germany--new",
            "sub1--Germany--same-name",
            "sub2--Japan--keep",
        ]);
        assert_eq!(replacements.len(), 1);
        assert!(conflicts.is_empty());
        assert_eq!(archive.added, 1);
        assert_eq!(archive.outbounds[0]["tag"], "same-name");
    }

    #[test]
    fn keeps_user_connection_when_matching_subscription_parameters_differ() {
        use std::collections::BTreeMap;
        use crate::subscriptions::Subscription;
        use super::merge_outbounds;

        let subscription = Subscription {
            id: "sub1".to_owned(),
            name: None,
            url: "https://example.com/sub".to_owned(),
            prefix: "sub1--".to_owned(),
        };
        let working = vec![
            json!({"tag":"sub1--old","protocol":"vless"}),
            json!({"tag":"node","protocol":"vless","settings":{"id":"user"}}),
        ];
        let updated = BTreeMap::from([(
            1,
            vec![json!({"tag":"sub1--node","protocol":"vless","settings":{"id":"subscription"}})],
        )]);
        let mut archive = ArchivePlan { outbounds: Vec::new(), added: 0 };

        let (final_outbounds, replacements, conflicts) =
            merge_outbounds(&working, &[subscription], &updated, &mut archive).unwrap();

        let tags: Vec<&str> = final_outbounds.iter()
            .map(|item| item["tag"].as_str().unwrap()).collect();
        assert_eq!(tags, vec!["sub1--node", "node"]);
        assert!(replacements.is_empty());
        assert_eq!(conflicts.len(), 1);
        assert_eq!(archive.added, 0);
    }
}
