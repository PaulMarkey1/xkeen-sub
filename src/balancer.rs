use serde_json::{Map, Value};
use std::collections::HashSet;
use std::error::Error;

type AppResult<T> = Result<T, Box<dyn Error>>;

pub const PROGRAM_BALANCER_TAG: &str = "xkeen-sub-balancer";
const RESERVE_BALANCER_TAG: &str = "xkeen-sub-balancer-rezerve";
const REST_BALANCER_TAG: &str = "xkeen-sub-rest";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BalancingMode {
    NotConfigured,
    Simple,
    Advanced,
    Custom,
    IncompleteAdvanced,
    Ambiguous,
}

/// Detects the active balancing mode from the actual Xray routing configuration.
pub fn detect_balancing_mode(document: &Value) -> BalancingMode {
    let Some(balancers) = document.get("routing")
        .and_then(|routing| routing.get("balancers"))
        .and_then(Value::as_array) else {
        return BalancingMode::NotConfigured;
    };
    if balancers.is_empty() {
        return BalancingMode::NotConfigured;
    }

    let managed_tags = [PROGRAM_BALANCER_TAG, RESERVE_BALANCER_TAG, REST_BALANCER_TAG];
    let mut found = HashSet::new();
    let mut has_custom = false;
    let mut duplicate = false;

    for balancer in balancers {
        let Some(tag) = balancer.get("tag").and_then(Value::as_str) else {
            has_custom = true;
            continue;
        };
        if managed_tags.contains(&tag) {
            if !found.insert(tag) {
                duplicate = true;
            }
        } else {
            has_custom = true;
        }
    }

    if has_custom {
        return BalancingMode::Custom;
    }
    if duplicate {
        return BalancingMode::Ambiguous;
    }
    if !found.contains(PROGRAM_BALANCER_TAG) {
        return BalancingMode::Custom;
    }
    match found.len() {
        1 => BalancingMode::Simple,
        3 => BalancingMode::Advanced,
        2 => BalancingMode::IncompleteAdvanced,
        _ => BalancingMode::Ambiguous,
    }
}
const PROTECTED_PROTOCOLS: [&str; 4] = ["freedom", "blackhole", "dns", "loopback"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BalancerCase {
    NoExistingBalancers,
    OneExistingBalancer,
    MultipleExistingBalancers,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BalancerPreparation {
    pub case: BalancerCase,
    pub tag: String,
    pub created: bool,
    pub updated_existing: bool,
    pub transformed_rules: usize,
    /// Tags of pre-existing user-defined balancers. The program-managed
    /// balancer is intentionally excluded.
    pub user_balancer_tags: Vec<String>,
}

/// Prepares routing.balancers in memory. The caller owns persistence; this
/// function never reads or writes files.
pub fn prepare_balancers(
    document: &mut Value,
    working_outbounds: &[Value],
    final_tags: &[String],
    api_tags: &HashSet<String>,
) -> AppResult<BalancerPreparation> {
    let routing = document
        .as_object_mut()
        .and_then(|root| root.get_mut("routing"))
        .and_then(Value::as_object_mut)
        .ok_or("В конфигурации не найден объект routing.")?;

    let mut balancers = match routing.get_mut("balancers") {
        None => Vec::new(),
        Some(value) => value
            .as_array()
            .cloned()
            .ok_or("Поле routing.balancers должно быть массивом.")?,
    };

    let case = match balancers.len() {
        0 => BalancerCase::NoExistingBalancers,
        1 => BalancerCase::OneExistingBalancer,
        _ => BalancerCase::MultipleExistingBalancers,
    };

    let program_index = balancers.iter().position(|item| {
        item.get("tag").and_then(Value::as_str) == Some(PROGRAM_BALANCER_TAG)
    });

    let mut created = false;
    let mut updated_existing = false;
    let effective_tag = PROGRAM_BALANCER_TAG.to_owned();

    match case {
        BalancerCase::NoExistingBalancers => {
            let redirected_rules =
                redirect_rules_to_program_balancer(routing, working_outbounds, api_tags)?;
            let normalized_balancer_tags = normalize_existing_balancer_tags(routing)?;
            let transformed_rules = redirected_rules + normalized_balancer_tags;
            balancers.push(new_program_balancer(final_tags));
            created = true;
            routing.insert("balancers".to_owned(), Value::Array(balancers));
            return Ok(BalancerPreparation {
                case,
                tag: PROGRAM_BALANCER_TAG.to_owned(),
                created,
                updated_existing,
                transformed_rules,
                user_balancer_tags: Vec::new(),
            });
        }
        BalancerCase::OneExistingBalancer => {
            if let Some(index) = program_index {
                set_program_balancer(&mut balancers[index], final_tags)?;
                updated_existing = true;
            } else {
                // A balancer without the program-owned tag belongs to the user.
                // Never repurpose it, even when its selector currently covers all
                // subscription outbounds: add and maintain our own balancer instead.
                balancers.push(new_program_balancer(final_tags));
                created = true;
            }
        }
        BalancerCase::MultipleExistingBalancers => {
            if let Some(index) = program_index {
                set_program_balancer(&mut balancers[index], final_tags)?;
                updated_existing = true;
            } else {
                // User-defined balancers and routing rules remain untouched.
                balancers.push(new_program_balancer(final_tags));
                created = true;
            }
        }
    }

    let user_balancer_tags = balancers
        .iter()
        .filter_map(|item| item.get("tag").and_then(Value::as_str))
        .filter(|tag| *tag != PROGRAM_BALANCER_TAG)
        .map(str::to_owned)
        .collect();
    routing.insert("balancers".to_owned(), Value::Array(balancers));

    Ok(BalancerPreparation {
        case,
        tag: effective_tag,
        created,
        updated_existing,
        transformed_rules: 0,
        user_balancer_tags,
    })
}

fn new_program_balancer(tags: &[String]) -> Value {
    serde_json::json!({
        "tag": PROGRAM_BALANCER_TAG,
        "selector": tags,
        "fallbackTag": "direct",
        "strategy": { "type": "leastPing" }
    })
}

fn set_program_balancer(balancer: &mut Value, tags: &[String]) -> AppResult<()> {
    set_selector(balancer, tags)?;
    balancer.as_object_mut()
        .ok_or("Балансировщик должен быть JSON-объектом.")?
        .insert("fallbackTag".to_owned(), Value::String("direct".to_owned()));
    Ok(())
}

fn set_selector(balancer: &mut Value, tags: &[String]) -> AppResult<()> {
    let object = balancer
        .as_object_mut()
        .ok_or("Балансировщик должен быть JSON-объектом.")?;
    object.insert(
        "selector".to_owned(),
        Value::Array(tags.iter().cloned().map(Value::String).collect()),
    );
    Ok(())
}

fn normalize_existing_balancer_tags(routing: &mut Map<String, Value>) -> AppResult<usize> {
    let Some(rules_value) = routing.get_mut("rules") else {
        return Ok(0);
    };
    let rules = rules_value
        .as_array_mut()
        .ok_or("Поле routing.rules должно быть массивом.")?;

    let mut changed = 0;
    for rule in rules {
        let Some(object) = rule.as_object_mut() else {
            continue;
        };
        let Some(balancer_tag) = object.get_mut("balancerTag") else {
            continue;
        };

        if balancer_tag.as_str() != Some(PROGRAM_BALANCER_TAG) {
            *balancer_tag = Value::String(PROGRAM_BALANCER_TAG.to_owned());
            changed += 1;
        }
    }

    Ok(changed)
}

fn redirect_rules_to_program_balancer(
    routing: &mut Map<String, Value>,
    working_outbounds: &[Value],
    api_tags: &HashSet<String>,
) -> AppResult<usize> {
    let Some(rules_value) = routing.get_mut("rules") else {
        return Ok(0);
    };
    let rules = rules_value
        .as_array_mut()
        .ok_or("Поле routing.rules должно быть массивом.")?;

    let protocols: std::collections::HashMap<&str, &str> = working_outbounds
        .iter()
        .filter_map(|outbound| {
            let object = outbound.as_object()?;
            Some((
                object.get("tag")?.as_str()?,
                object.get("protocol")?.as_str()?,
            ))
        })
        .collect();

    let mut changed = 0;
    for rule in rules {
        let Some(object) = rule.as_object_mut() else {
            continue;
        };
        let Some(outbound_tag) = object
            .get("outboundTag")
            .and_then(Value::as_str)
            .map(str::to_owned)
        else {
            continue;
        };

        if api_tags.contains(&outbound_tag) {
            continue;
        }

        if protocols
            .get(outbound_tag.as_str())
            .map(|protocol| PROTECTED_PROTOCOLS.contains(protocol))
            .unwrap_or(false)
        {
            continue;
        }

        object.remove("outboundTag");
        object.insert(
            "balancerTag".to_owned(),
            Value::String(PROGRAM_BALANCER_TAG.to_owned()),
        );
        changed += 1;
    }

    Ok(changed)
}

/// Removes blacklisted outbound tags only from the xkeen-sub-managed selector.
/// User-defined balancer selectors are intentionally left untouched.
pub(crate) fn filter_blacklisted_selectors(
    document: &mut Value,
    final_tags: &[String],
    blocked_tags: &HashSet<String>,
) -> AppResult<usize> {
    if blocked_tags.is_empty() {
        return Ok(0);
    }

    let balancers = document
        .as_object_mut()
        .and_then(|root| root.get_mut("routing"))
        .and_then(Value::as_object_mut)
        .and_then(|routing| routing.get_mut("balancers"))
        .and_then(Value::as_array_mut);
    let Some(balancers) = balancers else {
        return Ok(0);
    };

    let mut removed = 0;
    for balancer in balancers {
        // Blacklist maintenance is limited to the selector owned by xkeen-sub.
        // User-defined balancing policies must remain untouched.
        if balancer.get("tag").and_then(Value::as_str) != Some(PROGRAM_BALANCER_TAG) {
            continue;
        }
        let balancer_tag = PROGRAM_BALANCER_TAG.to_owned();
        let Some(selector) = balancer.get_mut("selector").and_then(Value::as_array_mut) else {
            continue;
        };

        let mut filtered = Vec::new();
        for value in selector.iter() {
            let Some(prefix) = value.as_str() else {
                filtered.push(value.clone());
                continue;
            };

            if blocked_tags.contains(prefix)
                || blocked_tags.iter().any(|blocked| blocked.starts_with(prefix))
            {
                // A selector is a prefix, not necessarily an exact tag. Expand it
                // to the remaining matching outbounds so blocking one exact tag
                // does not hide other allowed tags sharing the same prefix.
                let matches: Vec<&String> = final_tags
                    .iter()
                    .filter(|tag| tag.starts_with(prefix) && !blocked_tags.contains(*tag))
                    .collect();
                if matches.is_empty() {
                    removed += 1;
                } else {
                    removed += 1;
                    for tag in matches {
                        let item = Value::String(tag.clone());
                        if !filtered.contains(&item) {
                            filtered.push(item);
                        }
                    }
                }
            } else {
                filtered.push(value.clone());
            }
        }

        // Xray accepts selector prefixes that match no current outbound, but
        // such a selector is useless. Reject the blacklist change if no real,
        // eligible connection remains, even when the JSON array is non-empty.
        let has_matching_outbound = filtered.iter().filter_map(Value::as_str).any(|prefix| {
            final_tags.iter().any(|tag| tag.starts_with(prefix))
        });
        if !has_matching_outbound {
            return Err(format!(
                "Нельзя применить чёрный список: селектор балансировщика {balancer_tag} не содержит ни одного доступного подключения. Оставьте хотя бы одно подходящее подключение."
            ).into());
        }

        *selector = filtered;
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::{prepare_balancers, BalancerCase, PROGRAM_BALANCER_TAG};
    use serde_json::{json, Value};
    use std::collections::HashSet;

    fn working() -> Vec<Value> {
        vec![
            json!({"tag":"direct","protocol":"vless"}),
            json!({"tag":"my-freedom","protocol":"freedom"}),
            json!({"tag":"sub1--node","protocol":"vless"}),
        ]
    }

    fn tags() -> Vec<String> {
        vec!["sub1--node".to_owned(), "custom".to_owned()]
    }

    #[test]
    fn detects_simple_advanced_custom_and_incomplete_modes() {
        use super::{detect_balancing_mode as detect, BalancingMode};
        assert_eq!(detect(&json!({"routing":{"balancers":[]}})), BalancingMode::NotConfigured);
        assert_eq!(detect(&json!({"routing":{"balancers":[{"tag":PROGRAM_BALANCER_TAG}]}})), BalancingMode::Simple);
        assert_eq!(detect(&json!({"routing":{"balancers":[
            {"tag":PROGRAM_BALANCER_TAG},
            {"tag":"xkeen-sub-balancer-rezerve"},
            {"tag":"xkeen-sub-rest"}
        ]}})), BalancingMode::Advanced);
        assert_eq!(detect(&json!({"routing":{"balancers":[
            {"tag":PROGRAM_BALANCER_TAG},
            {"tag":"xkeen-sub-rest"}
        ]}})), BalancingMode::IncompleteAdvanced);
        assert_eq!(detect(&json!({"routing":{"balancers":[{"tag":"user-balancer"}]}})), BalancingMode::Custom);
        assert_eq!(detect(&json!({"routing":{"balancers":[
            {"tag":PROGRAM_BALANCER_TAG},{"tag":"user-balancer"}
        ]}})), BalancingMode::Custom);
    }

    #[test]
    fn blacklist_rejects_emptying_a_balancer_selector() {
        let mut config = json!({
            "routing": {
                "balancers": [{
                    "tag": PROGRAM_BALANCER_TAG,
                    "selector": ["sub1--"]
                }]
            }
        });
        let blocked = HashSet::from(["sub1--node".to_owned()]);
        let result = super::filter_blacklisted_selectors(
            &mut config,
            &[],
            &blocked,
        );
        assert!(result.is_err());
        assert_eq!(config["routing"]["balancers"][0]["selector"], json!(["sub1--"]));
    }

    #[test]
    fn blacklist_rejects_nonempty_selector_without_any_matching_outbound() {
        let mut config = json!({
            "routing": {
                "balancers": [{
                    "tag": PROGRAM_BALANCER_TAG,
                    "selector": ["missing-prefix"]
                }]
            }
        });
        let blocked = HashSet::from(["sub1--blocked".to_owned()]);
        let final_tags = vec!["sub1--allowed".to_owned()];
        let result = super::filter_blacklisted_selectors(&mut config, &final_tags, &blocked);
        assert!(result.is_err());
        assert_eq!(
            config["routing"]["balancers"][0]["selector"],
            json!(["missing-prefix"])
        );
    }

    #[test]
    fn blacklist_expands_exact_blocked_tag_to_other_matching_outbounds() {
        let mut config = json!({
            "routing": {
                "balancers": [{
                    "tag": PROGRAM_BALANCER_TAG,
                    "selector": ["sub1--node"]
                }]
            }
        });
        let blocked = HashSet::from(["sub1--node".to_owned()]);
        let final_tags = vec!["sub1--node-backup".to_owned()];
        super::filter_blacklisted_selectors(&mut config, &final_tags, &blocked).unwrap();
        assert_eq!(
            config["routing"]["balancers"][0]["selector"],
            json!(["sub1--node-backup"])
        );
    }

    #[test]
    fn blacklist_keeps_remaining_matches_in_prefix_selector() {
        let mut config = json!({
            "routing": {
                "balancers": [{
                    "tag": PROGRAM_BALANCER_TAG,
                    "selector": ["sub1--"]
                }]
            }
        });
        let blocked = HashSet::from(["sub1--blocked".to_owned()]);
        let final_tags = vec!["sub1--allowed".to_owned()];
        super::filter_blacklisted_selectors(&mut config, &final_tags, &blocked).unwrap();
        assert_eq!(config["routing"]["balancers"][0]["selector"], json!(["sub1--allowed"]));
    }

    #[test]
    fn case_one_creates_balancer_and_redirects_only_non_protected_protocols() {
        let mut config = json!({
            "routing": {
                "rules": [
                    {"outboundTag":"direct"},
                    {"outboundTag":"my-freedom"},
                    {"outboundTag":"unknown"},
                    {"outboundTag":"api-tag"}
                ]
            }
        });
        let api: HashSet<String> = ["api-tag".to_owned()].into_iter().collect();

        let result = prepare_balancers(&mut config, &working(), &tags(), &api).unwrap();

        assert_eq!(result.case, BalancerCase::NoExistingBalancers);
        assert!(result.created);
        assert_eq!(result.transformed_rules, 2);
        assert_eq!(
            config["routing"]["rules"][0]["balancerTag"],
            PROGRAM_BALANCER_TAG
        );
        assert_eq!(
            config["routing"]["rules"][1]["outboundTag"],
            "my-freedom"
        );
        assert_eq!(
            config["routing"]["rules"][2]["balancerTag"],
            PROGRAM_BALANCER_TAG
        );
        assert_eq!(
            config["routing"]["rules"][3]["outboundTag"],
            "api-tag"
        );
        assert_eq!(
            config["routing"]["balancers"][0]["selector"],
            json!(["sub1--node", "custom"])
        );
        assert_eq!(config["routing"]["balancers"][0]["fallbackTag"], "direct");
    }

    #[test]
    fn missing_balancers_redirects_existing_balancer_tags_to_program_balancer() {
        let mut config = json!({
            "routing": {
                "rules": [
                    {"type":"field","balancerTag":"old-balancer","domain":["example.com"]},
                    {"type":"field","balancerTag":"another-balancer","ip":["1.2.3.4"]},
                    {"type":"field","balancerTag":PROGRAM_BALANCER_TAG,"network":"tcp"}
                ]
            }
        });

        let result =
            prepare_balancers(&mut config, &working(), &tags(), &HashSet::new()).unwrap();

        assert_eq!(result.case, BalancerCase::NoExistingBalancers);
        assert!(result.created);
        assert_eq!(result.transformed_rules, 2);
        assert_eq!(
            config["routing"]["rules"][0],
            json!({"type":"field","balancerTag":PROGRAM_BALANCER_TAG,"domain":["example.com"]})
        );
        assert_eq!(
            config["routing"]["rules"][1],
            json!({"type":"field","balancerTag":PROGRAM_BALANCER_TAG,"ip":["1.2.3.4"]})
        );
        assert_eq!(
            config["routing"]["rules"][2],
            json!({"type":"field","balancerTag":PROGRAM_BALANCER_TAG,"network":"tcp"})
        );
        assert_eq!(
            config["routing"]["balancers"][0]["tag"],
            PROGRAM_BALANCER_TAG
        );
    }

    #[test]
    fn empty_balancers_normalizes_balancer_tags_and_keeps_outbound_redirect_behavior() {
        let mut config = json!({
            "routing": {
                "balancers": [],
                "rules": [
                    {"balancerTag":"missing-user-balancer","domain":["example.com"]},
                    {"outboundTag":"direct","network":"tcp"},
                    {"outboundTag":"my-freedom"},
                    {"outboundTag":"api-tag"}
                ]
            }
        });
        let api: HashSet<String> = ["api-tag".to_owned()].into_iter().collect();

        let result = prepare_balancers(&mut config, &working(), &tags(), &api).unwrap();

        assert_eq!(result.transformed_rules, 2);
        assert_eq!(
            config["routing"]["rules"][0]["balancerTag"],
            PROGRAM_BALANCER_TAG
        );
        assert_eq!(
            config["routing"]["rules"][1]["balancerTag"],
            PROGRAM_BALANCER_TAG
        );
        assert_eq!(config["routing"]["rules"][1]["network"], "tcp");
        assert_eq!(config["routing"]["rules"][1].get("outboundTag"), None);
        assert_eq!(config["routing"]["rules"][2]["outboundTag"], "my-freedom");
        assert_eq!(config["routing"]["rules"][3]["outboundTag"], "api-tag");
    }

    #[test]
    fn single_user_balancer_is_preserved_and_program_balancer_is_added() {
        let mut config = json!({
            "routing": {
                "rules": [],
                "balancers": [{
                    "tag":"user-balancer",
                    "selector":["direct","my-freedom","sub1--node","extra"],
                    "strategy":{"type":"random"}
                }]
            }
        });
        let original = config["routing"]["balancers"][0].clone();

        let result =
            prepare_balancers(&mut config, &working(), &tags(), &HashSet::new()).unwrap();

        assert_eq!(result.case, BalancerCase::OneExistingBalancer);
        assert_eq!(result.tag, PROGRAM_BALANCER_TAG);
        assert!(result.created);
        assert!(!result.updated_existing);
        assert_eq!(config["routing"]["balancers"].as_array().unwrap().len(), 2);
        assert_eq!(config["routing"]["balancers"][0], original);
        assert_eq!(config["routing"]["balancers"][1]["tag"], PROGRAM_BALANCER_TAG);
        assert_eq!(config["routing"]["balancers"][1]["selector"], json!(["sub1--node", "custom"]));
    }

    #[test]
    fn user_balancer_prefix_selector_is_not_repurposed() {
        let mut config = json!({
            "routing": {
                "rules": [],
                "balancers": [{
                    "tag":"user-balancer",
                    "selector":["sub1--", "direct", "my-freedom"],
                    "strategy":{"type":"random"}
                }]
            }
        });
        let original = config["routing"]["balancers"][0].clone();

        let result =
            prepare_balancers(&mut config, &working(), &tags(), &HashSet::new()).unwrap();

        assert!(result.created);
        assert_eq!(config["routing"]["balancers"].as_array().unwrap().len(), 2);
        assert_eq!(config["routing"]["balancers"][0], original);
    }

    #[test]
    fn case_two_adds_program_balancer_when_selector_is_incomplete() {
        let mut config = json!({
            "routing": {
                "rules": [],
                "balancers": [{
                    "tag":"user-balancer",
                    "selector":["direct"],
                    "strategy":{"type":"random"}
                }]
            }
        });

        let result =
            prepare_balancers(&mut config, &working(), &tags(), &HashSet::new()).unwrap();

        assert!(result.created);
        assert_eq!(result.user_balancer_tags, vec!["user-balancer"]);
        assert_eq!(config["routing"]["balancers"].as_array().unwrap().len(), 2);
        assert_eq!(config["routing"]["balancers"][0]["tag"], "user-balancer");
        assert_eq!(
            config["routing"]["balancers"][1]["tag"],
            PROGRAM_BALANCER_TAG
        );
    }

    #[test]
    fn existing_program_balancer_is_updated_even_with_other_balancers() {
        let mut config = json!({
            "routing": {
                "rules": [],
                "balancers": [
                    {"tag":"user-a","selector":["a"],"strategy":{"type":"random"}},
                    {"tag":PROGRAM_BALANCER_TAG,"selector":["old"],"strategy":{"type":"leastPing"}}
                ]
            }
        });

        let result =
            prepare_balancers(&mut config, &working(), &tags(), &HashSet::new()).unwrap();

        assert!(!result.created);
        assert!(result.updated_existing);
        assert_eq!(config["routing"]["balancers"].as_array().unwrap().len(), 2);
        assert_eq!(
            config["routing"]["balancers"][1]["selector"],
            json!(["sub1--node", "custom"])
        );
        assert_eq!(config["routing"]["balancers"][1]["fallbackTag"], "direct");
    }

    #[test]
    fn case_one_adds_direct_fallback_to_new_program_balancer() {
        let mut config = json!({"routing":{"rules":[]}});
        prepare_balancers(&mut config, &working(), &tags(), &HashSet::new()).unwrap();
        assert_eq!(config["routing"]["balancers"][0]["fallbackTag"], "direct");
    }

    #[test]
    fn case_three_adds_program_balancer_without_touching_user_balancers() {
        let mut config = json!({
            "routing": {
                "rules": [{"outboundTag":"direct"}],
                "balancers": [
                    {"tag":"user-a","selector":["a"],"strategy":{"type":"random"}},
                    {"tag":"user-b","selector":["b"],"strategy":{"type":"roundRobin"}}
                ]
            }
        });
        let before = config["routing"]["balancers"].clone();

        let result =
            prepare_balancers(&mut config, &working(), &tags(), &HashSet::new()).unwrap();

        assert_eq!(result.case, BalancerCase::MultipleExistingBalancers);
        assert!(result.created);
        assert_eq!(result.user_balancer_tags, vec!["user-a", "user-b"]);
        assert_eq!(config["routing"]["balancers"][0], before[0]);
        assert_eq!(config["routing"]["balancers"][1], before[1]);
        assert_eq!(config["routing"]["rules"][0]["outboundTag"], "direct");
    }
    #[test]
    fn blacklist_does_not_modify_user_defined_balancer_selectors() {
        let mut config = json!({"routing":{"balancers":[
            {"tag":"user-balancer","selector":["sub1--"]},
            {"tag":PROGRAM_BALANCER_TAG,"selector":["sub1--"]}
        ]}});
        let blocked = HashSet::from(["sub1--node".to_owned()]);
        let final_tags = vec!["sub1--allowed".to_owned()];
        super::filter_blacklisted_selectors(&mut config, &final_tags, &blocked).unwrap();
        assert_eq!(config["routing"]["balancers"][0]["selector"], json!(["sub1--"]));
        assert_eq!(config["routing"]["balancers"][1]["selector"], json!(["sub1--allowed"]));
    }

    #[test]
    fn blacklist_removes_exact_tags_and_expands_prefix_selectors() {
        use super::filter_blacklisted_selectors;
        let mut config = json!({"routing":{"balancers":[{"tag":PROGRAM_BALANCER_TAG,"selector":["sub1--node","sub1--","custom"]}]}});
        let blocked: HashSet<String> = ["sub1--node".to_owned(), "sub1--node2".to_owned()].into_iter().collect();
        let final_tags = vec!["sub1--keep".to_owned(), "custom".to_owned()];
        let removed = filter_blacklisted_selectors(&mut config, &final_tags, &blocked).unwrap();
        assert_eq!(removed, 2);
        assert_eq!(config["routing"]["balancers"][0]["selector"], json!(["sub1--keep", "custom"]));
    }

}
