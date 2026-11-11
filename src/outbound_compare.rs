use serde_json::Value;
use std::collections::HashSet;
use std::error::Error;

type AppResult<T> = Result<T, Box<dyn Error>>;

#[derive(Debug, Clone, PartialEq)]
pub struct ParameterChange {
    pub path: String,
    pub previous: Option<Value>,
    pub current: Option<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChangedOutbound {
    pub tag: String,
    pub changes: Vec<ParameterChange>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TaggedOutbound {
    pub tag: String,
    pub outbound: Value,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ComparisonReport {
    pub added: Vec<TaggedOutbound>,
    pub removed: Vec<TaggedOutbound>,
    pub changed: Vec<ChangedOutbound>,
    pub unchanged: usize,
    pub unrecognized_previous: usize,
    pub unrecognized_current: usize,
    pub identical_duplicates_removed: usize,
}

/// Compares the outbounds arrays of two subscription JSON documents.
///
/// previous_json = None means that no baseline exists yet, so every
/// recognized outbound in the current document is reported as added.
/// The original documents are not modified. Unknown outbound parameters are
/// preserved because comparison operates on complete serde_json::Value objects.
pub fn compare_subscription(
    previous_json: Option<&[u8]>,
    current_json: &[u8],
) -> AppResult<ComparisonReport> {
    let (previous, unrecognized_previous, duplicates_previous) =
        match previous_json {
            Some(bytes) => normalize_document(bytes)?,
            None => (Vec::new(), 0, 0),
        };

    let (current, unrecognized_current, duplicates_current) =
        normalize_document(current_json)?;

    let mut report = ComparisonReport {
        unrecognized_previous,
        unrecognized_current,
        identical_duplicates_removed: duplicates_previous + duplicates_current,
        ..ComparisonReport::default()
    };

    let mut matched_current = vec![false; current.len()];

    for old in &previous {
        if let Some((index, new)) = current
            .iter()
            .enumerate()
            .find(|(index, new)| !matched_current[*index] && new.tag == old.tag)
        {
            matched_current[index] = true;
            let changes = parameter_changes(&old.outbound, &new.outbound);
            if changes.is_empty() {
                report.unchanged += 1;
            } else {
                report.changed.push(ChangedOutbound {
                    tag: old.tag.clone(),
                    changes,
                });
            }
        } else {
            report.removed.push(old.clone());
        }
    }

    for (index, new) in current.iter().enumerate() {
        if !matched_current[index] {
            report.added.push(new.clone());
        }
    }

    Ok(report)
}

fn normalize_document(bytes: &[u8]) -> AppResult<(Vec<TaggedOutbound>, usize, usize)> {
    let document: Value = serde_json::from_slice(bytes)
        .map_err(|error| format!("Некорректный JSON подписки: {error}"))?;

    let outbounds = document
        .get("outbounds")
        .and_then(Value::as_array)
        .ok_or("В JSON подписки отсутствует массив outbounds.")?;

    let mut groups: Vec<(String, Vec<Value>)> = Vec::new();
    let mut unrecognized = 0usize;
    let mut identical_duplicates_removed = 0usize;

    for outbound in outbounds {
        let Some(object) = outbound.as_object() else {
            unrecognized += 1;
            continue;
        };
        let Some(tag) = object.get("tag").and_then(Value::as_str) else {
            unrecognized += 1;
            continue;
        };
        if tag.is_empty() {
            unrecognized += 1;
            continue;
        }

        let group_index = groups.iter().position(|(existing_tag, _)| existing_tag == tag);
        let index = match group_index {
            Some(index) => index,
            None => {
                groups.push((tag.to_owned(), Vec::new()));
                groups.len() - 1
            }
        };

        if groups[index].1.iter().any(|existing| existing == outbound) {
            identical_duplicates_removed += 1;
        } else {
            groups[index].1.push(outbound.clone());
        }
    }

    // Reserve all source tags first. Generated suffixes must never collide
    // with a different tag that already exists in the same subscription.
    let reserved: HashSet<String> = groups.iter().map(|(tag, _)| tag.clone()).collect();
    let mut used = HashSet::new();
    let mut normalized = Vec::new();

    for (original_tag, outbounds_for_tag) in groups {
        for (duplicate_index, mut outbound) in outbounds_for_tag.into_iter().enumerate() {
            let assigned_tag = if duplicate_index == 0 {
                original_tag.clone()
            } else {
                let mut suffix = duplicate_index;
                loop {
                    let candidate = format!("{original_tag}--{suffix}");
                    if !reserved.contains(&candidate) && !used.contains(&candidate) {
                        break candidate;
                    }
                    suffix += 1;
                }
            };

            if !used.insert(assigned_tag.clone()) {
                return Err(format!(
                    "Не удалось назначить уникальный Tag для подключения {original_tag}."
                )
                .into());
            }

            let object = outbound
                .as_object_mut()
                .ok_or("Внутренняя ошибка: Outbound перестал быть JSON-объектом.")?;
            object.insert("tag".to_owned(), Value::String(assigned_tag.clone()));
            normalized.push(TaggedOutbound {
                tag: assigned_tag,
                outbound,
            });
        }
    }

    Ok((normalized, unrecognized, identical_duplicates_removed))
}

fn parameter_changes(previous: &Value, current: &Value) -> Vec<ParameterChange> {
    let mut changes = Vec::new();
    collect_changes("", previous, current, &mut changes);
    changes
}

fn collect_changes(
    path: &str,
    previous: &Value,
    current: &Value,
    changes: &mut Vec<ParameterChange>,
) {
    if previous == current {
        return;
    }

    if let (Some(old_object), Some(new_object)) = (previous.as_object(), current.as_object()) {
        let mut keys: Vec<&String> = old_object.keys().chain(new_object.keys()).collect();
        keys.sort();
        keys.dedup();

        for key in keys {
            // Tag is the comparison key, not a connection parameter.
            if path.is_empty() && key == "tag" {
                continue;
            }

            let old_value = old_object.get(key);
            let new_value = new_object.get(key);
            let child_path = if path.is_empty() {
                key.clone()
            } else {
                format!("{path}.{key}")
            };

            match (old_value, new_value) {
                (Some(old), Some(new)) => {
                    collect_changes(&child_path, old, new, changes);
                }
                (old, new) => changes.push(ParameterChange {
                    path: child_path,
                    previous: old.cloned(),
                    current: new.cloned(),
                }),
            }
        }
        return;
    }

    changes.push(ParameterChange {
        path: if path.is_empty() { "$".to_owned() } else { path.to_owned() },
        previous: Some(previous.clone()),
        current: Some(current.clone()),
    });
}

#[cfg(test)]
mod tests {
    use super::compare_subscription;
    use serde_json::json;

    fn document(outbounds: serde_json::Value) -> Vec<u8> {
        serde_json::to_vec(&json!({ "outbounds": outbounds })).unwrap()
    }

    #[test]
    fn first_download_reports_all_recognized_outbounds_as_added() {
        let current = document(json!([
            {"tag":"one","protocol":"freedom"},
            {"tag":"two","protocol":"blackhole"}
        ]));

        let report = compare_subscription(None, &current).unwrap();

        assert_eq!(report.added.len(), 2);
        assert!(report.removed.is_empty());
        assert_eq!(report.unchanged, 0);
    }

    #[test]
    fn compares_by_tag_and_reports_nested_parameter_changes() {
        let previous = document(json!([
            {"tag":"keep","protocol":"freedom"},
            {"tag":"change","protocol":"vless","settings":{"vnext":[{"port":443}]}}
        ]));
        let current = document(json!([
            {"tag":"keep","protocol":"freedom"},
            {"tag":"change","protocol":"vless","settings":{"vnext":[{"port":8443}]}}
        ]));

        let report = compare_subscription(Some(&previous), &current).unwrap();

        assert_eq!(report.unchanged, 1);
        assert_eq!(report.changed.len(), 1);
        assert_eq!(report.changed[0].tag, "change");
        assert_eq!(report.changed[0].changes[0].path, "settings.vnext");
        assert!(report.added.is_empty());
        assert!(report.removed.is_empty());
    }

    #[test]
    fn changed_tag_is_reported_as_removal_and_addition() {
        let previous = document(json!([{"tag":"old","protocol":"freedom"}]));
        let current = document(json!([{"tag":"new","protocol":"freedom"}]));

        let report = compare_subscription(Some(&previous), &current).unwrap();

        assert_eq!(report.removed[0].tag, "old");
        assert_eq!(report.added[0].tag, "new");
        assert!(report.changed.is_empty());
    }

    #[test]
    fn identical_duplicate_tags_are_collapsed() {
        let current = document(json!([
            {"tag":"same","protocol":"freedom"},
            {"tag":"same","protocol":"freedom"}
        ]));

        let report = compare_subscription(None, &current).unwrap();

        assert_eq!(report.added.len(), 1);
        assert_eq!(report.identical_duplicates_removed, 1);
    }

    #[test]
    fn different_duplicate_tags_receive_sequential_suffixes() {
        let current = document(json!([
            {"tag":"same","protocol":"freedom"},
            {"tag":"same","protocol":"blackhole"},
            {"tag":"same","protocol":"socks"}
        ]));

        let report = compare_subscription(None, &current).unwrap();

        let tags: Vec<&str> = report.added.iter().map(|item| item.tag.as_str()).collect();
        assert_eq!(tags, vec!["same", "same--1", "same--2"]);
    }

    #[test]
    fn generated_suffix_does_not_collide_with_an_existing_source_tag() {
        let current = document(json!([
            {"tag":"same","protocol":"freedom"},
            {"tag":"same","protocol":"blackhole"},
            {"tag":"same--1","protocol":"socks"}
        ]));

        let report = compare_subscription(None, &current).unwrap();

        let tags: Vec<&str> = report.added.iter().map(|item| item.tag.as_str()).collect();
        assert_eq!(tags, vec!["same", "same--2", "same--1"]);
    }

    #[test]
    fn unrecognized_entries_are_counted_and_not_added() {
        let current = document(json!([
            {"tag":"valid","protocol":"freedom"},
            {"protocol":"freedom"},
            "not-an-object",
            {"tag":""}
        ]));

        let report = compare_subscription(None, &current).unwrap();

        assert_eq!(report.added.len(), 1);
        assert_eq!(report.unrecognized_current, 3);
    }

    #[test]
    fn malformed_document_or_missing_outbounds_is_an_error() {
        assert!(compare_subscription(None, br#"{"outbounds":"wrong"}"#).is_err());
        assert!(compare_subscription(None, b"{").is_err());
    }

    #[test]
    fn unknown_parameters_are_preserved_and_compared() {
        let previous = document(json!([
            {"tag":"one","protocol":"vless","customFutureField":{"value":1}}
        ]));
        let current = document(json!([
            {"tag":"one","protocol":"vless","customFutureField":{"value":2}}
        ]));

        let report = compare_subscription(Some(&previous), &current).unwrap();

        assert_eq!(report.changed.len(), 1);
        assert_eq!(report.changed[0].changes[0].path, "customFutureField.value");
    }
}
