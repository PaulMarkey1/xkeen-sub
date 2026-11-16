use crate::outbound_compare::outbound_parameters_equal;
use crate::subscription_identity::number_from_tag;
pub(crate) use crate::subscription_identity::number_from_id as subscription_id_number;
use crate::subscriptions::Subscription;
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::error::Error;
use std::fs;
use std::path::Path;

type AppResult<T> = Result<T, Box<dyn Error>>;

pub(crate) const USER_ARCHIVE: &str = "/opt/etc/xkeen-sub/old-user-outbound.json";

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
        if let Some(number) = number_from_tag(tag) {
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

/// Rebuilds all subscription-managed groups from baseline data while preserving user outbounds.
pub(crate) fn rebuild_outbounds(
    working: &[Value],
    subscriptions: &[Subscription],
    rebuilt: &BTreeMap<u64, Vec<Value>>,
) -> AppResult<Vec<Value>> {
    let mut user_outbounds = Vec::new();
    for outbound in working {
        let tag = outbound.get("tag").and_then(Value::as_str)
            .ok_or("В Рабочем Outbound найден элемент без Tag.")?;
        if number_from_tag(tag).is_none() {
            user_outbounds.push(outbound.clone());
        }
    }

    let mut result = Vec::new();
    let mut expected_numbers = HashSet::new();
    for subscription in subscriptions {
        let number = subscription_id_number(&subscription.id)
            .ok_or_else(|| format!("Некорректный ID подписки: {}.", subscription.id))?;
        if !expected_numbers.insert(number) {
            return Err(format!("Повторяется номер подписки: {}.", subscription.id).into());
        }
        if let Some(group) = rebuilt.get(&number) {
            result.extend(group.iter().cloned());
        }
    }

    for number in rebuilt.keys() {
        if !expected_numbers.contains(number) {
            return Err(format!("Подготовлена группа для отсутствующей подписки sub{number}.").into());
        }
    }

    result.extend(user_outbounds);
    collect_tags(&result)?;
    Ok(result)
}

/// Ensures the final outbound list contains one outbound for each required service protocol.
/// Existing service outbounds are preserved; missing ones are appended with collision-free tags.
pub(crate) fn ensure_service_outbounds(outbounds: &mut Vec<Value>) -> AppResult<Vec<String>> {
    let mut tags = HashSet::new();
    for outbound in outbounds.iter() {
        let tag = outbound.get("tag").and_then(Value::as_str)
            .filter(|tag| !tag.is_empty())
            .ok_or("Итоговый Outbound содержит пустой или отсутствующий Tag.")?;
        tags.insert(tag.to_owned());
    }

    let mut added = Vec::new();
    for (protocol, base_tag) in [("freedom", "direct"), ("blackhole", "block")] {
        if outbounds.iter().any(|outbound| {
            outbound.get("protocol").and_then(Value::as_str) == Some(protocol)
        }) {
            continue;
        }

        let mut tag = base_tag.to_owned();
        let mut suffix = 1u64;
        while tags.contains(&tag) {
            tag = format!("{base_tag}{suffix}");
            suffix += 1;
        }

        let mut outbound = serde_json::Map::new();
        outbound.insert("protocol".to_owned(), Value::String(protocol.to_owned()));
        outbound.insert("tag".to_owned(), Value::String(tag.clone()));
        outbounds.push(Value::Object(outbound));
        tags.insert(tag.clone());
        added.push(format!("{protocol}:{tag}"));
    }

    Ok(added)
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

/// Returns only outbound tags that are eligible for Observatory and balancer selectors.
/// Service outbounds and the Xray API outbound are excluded by outbound type/tag.
pub(crate) fn collect_selector_tags(
    outbounds: &[Value],
    api_tags: &HashSet<String>,
) -> AppResult<Vec<String>> {
    const EXCLUDED_PROTOCOLS: [&str; 4] = ["dns", "loopback", "freedom", "blackhole"];

    let mut tags = Vec::new();
    for outbound in outbounds {
        let tag = outbound.get("tag").and_then(Value::as_str)
            .filter(|tag| !tag.is_empty())
            .ok_or("Итоговый Outbound содержит пустой или отсутствующий Tag.")?;
        let protocol = outbound.get("protocol").and_then(Value::as_str).unwrap_or_default();

        if api_tags.contains(tag) || EXCLUDED_PROTOCOLS.contains(&protocol) {
            continue;
        }
        tags.push(tag.to_owned());
    }
    Ok(tags)
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
    use super::{archive_outbound, collect_selector_tags, rebuild_outbounds, ArchivePlan};
    use crate::subscription_identity::{number_from_id, number_from_tag};
    use crate::outbound_compare::outbound_parameters_equal;
    use serde_json::json;

    #[test]
    fn adds_missing_service_outbounds_with_base_tags() {
        let mut outbounds = vec![json!({"tag":"node","protocol":"vless"})];
        let added = super::ensure_service_outbounds(&mut outbounds).unwrap();
        assert_eq!(added, vec!["freedom:direct", "blackhole:block"]);
        assert_eq!(outbounds[1], json!({"protocol":"freedom","tag":"direct"}));
        assert_eq!(outbounds[2], json!({"protocol":"blackhole","tag":"block"}));
    }

    #[test]
    fn adds_suffix_when_service_tag_is_used_by_another_protocol() {
        let mut outbounds = vec![
            json!({"tag":"direct","protocol":"vless"}),
            json!({"tag":"block","protocol":"socks"}),
            json!({"tag":"direct1","protocol":"trojan"}),
        ];
        super::ensure_service_outbounds(&mut outbounds).unwrap();
        assert_eq!(outbounds[3], json!({"protocol":"freedom","tag":"direct2"}));
        assert_eq!(outbounds[4], json!({"protocol":"blackhole","tag":"block1"}));
    }

    #[test]
    fn keeps_existing_service_protocols_even_with_custom_tags() {
        let mut outbounds = vec![
            json!({"tag":"my-direct","protocol":"freedom","settings":{"domainStrategy":"UseIPv4"}}),
            json!({"tag":"my-block","protocol":"blackhole"}),
        ];
        let added = super::ensure_service_outbounds(&mut outbounds).unwrap();
        assert!(added.is_empty());
        assert_eq!(outbounds.len(), 2);
        assert_eq!(outbounds[0]["settings"]["domainStrategy"], "UseIPv4");
    }

    #[test]
    fn service_outbounds_are_appended_after_existing_entries() {
        let mut outbounds = vec![
            json!({"tag":"first","protocol":"vless"}),
            json!({"tag":"second","protocol":"socks"}),
        ];
        super::ensure_service_outbounds(&mut outbounds).unwrap();
        assert_eq!(outbounds[0]["tag"], "first");
        assert_eq!(outbounds[1]["tag"], "second");
        assert_eq!(outbounds[2]["tag"], "direct");
        assert_eq!(outbounds[3]["tag"], "block");
    }

    #[test]
    fn selector_tags_exclude_service_protocols_and_api_tag() {
        use std::collections::HashSet;
        let outbounds = vec![
            json!({"tag":"dns-out","protocol":"dns"}),
            json!({"tag":"loop","protocol":"loopback"}),
            json!({"tag":"direct","protocol":"freedom"}),
            json!({"tag":"reject","protocol":"blackhole"}),
            json!({"tag":"api-out","protocol":"dokodemo-door"}),
            json!({"tag":"vless-node","protocol":"vless"}),
            json!({"tag":"socks-node","protocol":"socks"}),
        ];
        let api_tags: HashSet<String> = ["api-out".to_owned()].into_iter().collect();

        let tags = collect_selector_tags(&outbounds, &api_tags).unwrap();

        assert_eq!(tags, vec!["vless-node", "socks-node"]);
    }

    #[test]
    fn identifies_subscription_tags_and_ids() {
        assert_eq!(number_from_tag("sub1--Germany--node"), Some(1));
        assert_eq!(number_from_id("sub20"), Some(20));
        assert_eq!(number_from_tag("sub20--node"), Some(20));
        assert_eq!(number_from_tag("custom"), None);
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
    fn full_rebuild_drops_old_subscription_groups_and_preserves_user_outbounds() {
        use std::collections::BTreeMap;
        use crate::subscriptions::Subscription;

        let subscriptions = vec![
            Subscription {
                id: "sub1".to_owned(),
                name: Some("Japan".to_owned()),
                url: "https://example.com/one".to_owned(),
                prefix: "sub1--Japan--".to_owned(),
            },
        ];
        let working = vec![
            json!({"tag":"sub1--old","protocol":"vless"}),
            json!({"tag":"sub2--removed","protocol":"vless"}),
            json!({"tag":"custom-user","protocol":"socks"}),
        ];
        let rebuilt = BTreeMap::from([(
            1,
            vec![json!({"tag":"sub1--Japan--new","protocol":"vless"})],
        )]);

        let result = rebuild_outbounds(&working, &subscriptions, &rebuilt).unwrap();
        let tags: Vec<&str> = result.iter()
            .map(|item| item["tag"].as_str().unwrap()).collect();

        assert_eq!(tags, vec!["sub1--Japan--new", "custom-user"]);
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
