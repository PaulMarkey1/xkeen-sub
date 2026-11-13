use serde_json::{Map, Value};
use std::collections::HashSet;
use std::error::Error;

type AppResult<T> = Result<T, Box<dyn Error>>;

pub const PROGRAM_BALANCER_TAG: &str = "xkeen-sub-balancer";
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

    match case {
        BalancerCase::NoExistingBalancers => {
            let transformed_rules =
                redirect_rules_to_program_balancer(routing, working_outbounds, api_tags)?;
            balancers.push(new_program_balancer(final_tags));
            created = true;
            routing.insert("balancers".to_owned(), Value::Array(balancers));
            return Ok(BalancerPreparation {
                case,
                tag: PROGRAM_BALANCER_TAG.to_owned(),
                created,
                updated_existing,
                transformed_rules,
            });
        }
        BalancerCase::OneExistingBalancer => {
            if let Some(index) = program_index {
                set_selector(&mut balancers[index], final_tags)?;
                updated_existing = true;
            } else {
                let selector = balancers[0].get("selector").and_then(Value::as_array);
                let working_tags: HashSet<&str> = working_outbounds
                    .iter()
                    .filter_map(|outbound| outbound.get("tag").and_then(Value::as_str))
                    .collect();
                let selector_tags: HashSet<&str> = selector
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect();

                if selector.is_some() && working_tags.iter().all(|tag| selector_tags.contains(tag)) {
                    // Extra selector entries are allowed; the complete selector
                    // is replaced with the final outbound list.
                    set_selector(&mut balancers[0], final_tags)?;
                    updated_existing = true;
                } else {
                    balancers.push(new_program_balancer(final_tags));
                    created = true;
                }
            }
        }
        BalancerCase::MultipleExistingBalancers => {
            if let Some(index) = program_index {
                set_selector(&mut balancers[index], final_tags)?;
                updated_existing = true;
            } else {
                // User-defined balancers and routing rules remain untouched.
                balancers.push(new_program_balancer(final_tags));
                created = true;
            }
        }
    }

    routing.insert("balancers".to_owned(), Value::Array(balancers));
    Ok(BalancerPreparation {
        case,
        tag: PROGRAM_BALANCER_TAG.to_owned(),
        created,
        updated_existing,
        transformed_rules: 0,
    })
}

fn new_program_balancer(tags: &[String]) -> Value {
    serde_json::json!({
        "tag": PROGRAM_BALANCER_TAG,
        "selector": tags,
        "strategy": { "type": "leastPing" }
    })
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
    }

    #[test]
    fn case_two_updates_existing_selector_when_it_contains_all_working_tags() {
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

        let result =
            prepare_balancers(&mut config, &working(), &tags(), &HashSet::new()).unwrap();

        assert_eq!(result.case, BalancerCase::OneExistingBalancer);
        assert!(!result.created);
        assert!(result.updated_existing);
        assert_eq!(config["routing"]["balancers"].as_array().unwrap().len(), 1);
        assert_eq!(
            config["routing"]["balancers"][0]["selector"],
            json!(["sub1--node", "custom"])
        );
        assert_eq!(config["routing"]["balancers"][0]["strategy"]["type"], "random");
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
        assert_eq!(config["routing"]["balancers"].as_array().unwrap().len(), 2);
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
        assert_eq!(config["routing"]["balancers"][0], before[0]);
        assert_eq!(config["routing"]["balancers"][1], before[1]);
        assert_eq!(config["routing"]["rules"][0]["outboundTag"], "direct");
    }
}
