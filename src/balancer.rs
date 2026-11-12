use serde_json::Value;
use std::error::Error;

type AppResult<T> = Result<T, Box<dyn Error>>;

/// Returns true when a top-level Xray Observatory or Burst Observatory
/// configuration with a subjectSelector is present.
pub fn has_observatory(document: &Value) -> bool {
    let Some(root) = document.as_object() else {
        return false;
    };

    ["observatory", "burstObservatory", "burstobservatory"]
        .iter()
        .any(|key| {
            root.get(*key)
                .and_then(Value::as_object)
                .and_then(|object| object.get("subjectSelector"))
                .map(Value::is_array)
                .unwrap_or(false)
        })
}

/// Replaces selectors in every routing balancer with the final outbound Tag
/// list. All balancers are validated before any selector is changed, so an
/// error leaves the supplied document untouched.
///
/// Xray's leastPing and leastLoad strategies require an Observatory feature.
/// Either the standard Observatory or Burst Observatory satisfies that
/// requirement. random and roundRobin do not require an Observatory.
pub fn update_balancer_selectors(
    document: &mut Value,
    tags: &[String],
    observatory_available: bool,
) -> AppResult<usize> {
    let routing = document
        .as_object_mut()
        .and_then(|root| root.get_mut("routing"))
        .and_then(Value::as_object_mut)
        .ok_or("В конфигурации не найден объект routing.")?;

    let balancers = routing
        .get_mut("balancers")
        .and_then(Value::as_array_mut)
        .ok_or("В объекте routing не найден массив balancers.")?;

    if balancers.is_empty() {
        return Err("Массив routing.balancers пуст.".into());
    }

    // Validate every balancer first. Do not partially update the array.
    for (index, balancer) in balancers.iter().enumerate() {
        let object = balancer.as_object().ok_or_else(|| {
            format!("Балансировщик №{} должен быть объектом.", index + 1)
        })?;

        match object.get("selector") {
            Some(Value::Array(_)) => {}
            Some(_) => {
                return Err(format!(
                    "У балансировщика №{} поле selector должно быть массивом.",
                    index + 1
                )
                .into());
            }
            None => {
                return Err(format!(
                    "У балансировщика №{} отсутствует поле selector.",
                    index + 1
                )
                .into());
            }
        }

        let strategy_type = match object.get("strategy") {
            None => "random",
            Some(Value::Object(strategy)) => match strategy.get("type") {
                None => "random",
                Some(Value::String(value)) => value.as_str(),
                Some(_) => {
                    return Err(format!(
                        "У балансировщика №{} поле strategy.type должно быть строкой.",
                        index + 1
                    )
                    .into());
                }
            },
            Some(_) => {
                return Err(format!(
                    "У балансировщика №{} поле strategy должно быть объектом.",
                    index + 1
                )
                .into());
            }
        };

        match strategy_type {
            "random" | "roundRobin" => {}
            "leastPing" | "leastLoad" if observatory_available => {}
            "leastPing" | "leastLoad" => {
                return Err(format!(
                    "Стратегия {strategy_type} балансировщика №{} требует Observatory или Burst Observatory, но они не найдены.",
                    index + 1
                )
                .into());
            }
            other => {
                return Err(format!(
                    "У балансировщика №{} неизвестная стратегия: {other}.",
                    index + 1
                )
                .into());
            }
        }
    }

    let updated_count = balancers.len();
    let selector = Value::Array(tags.iter().cloned().map(Value::String).collect());

    for balancer in balancers.iter_mut() {
        // Safe because the complete array was validated above.
        balancer
            .as_object_mut()
            .expect("validated balancer object")
            .insert("selector".to_owned(), selector.clone());
    }

    Ok(updated_count)
}

#[cfg(test)]
mod tests {
    use super::{has_observatory, update_balancer_selectors};
    use serde_json::{json, Value};

    fn tags() -> Vec<String> {
        vec!["sub1--node-a".to_owned(), "custom-node".to_owned()]
    }

    #[test]
    fn replaces_selectors_in_every_balancer_and_preserves_other_fields() {
        let mut config = json!({
            "routing": {
                "balancers": [
                    {
                        "tag": "first",
                        "selector": ["old-a"],
                        "strategy": {"type": "leastPing"}
                    },
                    {
                        "tag": "second",
                        "selector": ["old-b"],
                        "strategy": {"type": "roundRobin"},
                        "fallbackTag": "direct"
                    }
                ]
            }
        });

        assert_eq!(
            update_balancer_selectors(&mut config, &tags(), true).unwrap(),
            2
        );
        assert_eq!(
            config["routing"]["balancers"][0]["selector"],
            json!(["sub1--node-a", "custom-node"])
        );
        assert_eq!(
            config["routing"]["balancers"][1]["selector"],
            json!(["sub1--node-a", "custom-node"])
        );
        assert_eq!(config["routing"]["balancers"][1]["fallbackTag"], "direct");
    }

    #[test]
    fn missing_routing_is_an_error_and_does_not_change_document() {
        let mut config = json!({"outbounds": []});
        let before = config.clone();

        assert!(update_balancer_selectors(&mut config, &tags(), true).is_err());
        assert_eq!(config, before);
    }

    #[test]
    fn missing_balancers_is_an_error_and_does_not_change_document() {
        let mut config = json!({"routing": {"rules": []}});
        let before = config.clone();

        assert!(update_balancer_selectors(&mut config, &tags(), true).is_err());
        assert_eq!(config, before);
    }

    #[test]
    fn empty_balancers_is_an_error() {
        let mut config = json!({"routing": {"balancers": []}});
        assert!(update_balancer_selectors(&mut config, &tags(), true).is_err());
    }

    #[test]
    fn missing_selector_in_any_balancer_prevents_all_changes() {
        let mut config = json!({
            "routing": {
                "balancers": [
                    {"tag": "first", "selector": ["old"], "strategy": {"type": "random"}},
                    {"tag": "second", "strategy": {"type": "random"}}
                ]
            }
        });
        let before = config.clone();

        assert!(update_balancer_selectors(&mut config, &tags(), true).is_err());
        assert_eq!(config, before);
    }

    #[test]
    fn least_ping_requires_observatory_and_failure_is_atomic() {
        let mut config = json!({
            "routing": {
                "balancers": [
                    {"tag": "first", "selector": ["old"], "strategy": {"type": "leastPing"}}
                ]
            }
        });
        let before = config.clone();

        assert!(update_balancer_selectors(&mut config, &tags(), false).is_err());
        assert_eq!(config, before);
    }

    #[test]
    fn least_load_requires_observatory_and_accepts_standard_observatory() {
        let mut config = json!({
            "routing": {
                "balancers": [
                    {"tag": "first", "selector": ["old"], "strategy": {"type": "leastLoad"}}
                ]
            }
        });

        assert_eq!(update_balancer_selectors(&mut config, &tags(), true).unwrap(), 1);
    }

    #[test]
    fn random_and_round_robin_do_not_require_observatory() {
        let mut config = json!({
            "routing": {
                "balancers": [
                    {"tag": "first", "selector": ["old"], "strategy": {"type": "random"}},
                    {"tag": "second", "selector": ["old"], "strategy": {"type": "roundRobin"}}
                ]
            }
        });

        assert_eq!(update_balancer_selectors(&mut config, &tags(), false).unwrap(), 2);
    }

    #[test]
    fn unknown_strategy_is_an_error_and_does_not_change_document() {
        let mut config = json!({
            "routing": {
                "balancers": [
                    {"tag": "first", "selector": ["old"], "strategy": {"type": "futureStrategy"}}
                ]
            }
        });
        let before = config.clone();

        assert!(update_balancer_selectors(&mut config, &tags(), true).is_err());
        assert_eq!(config, before);
    }

    #[test]
    fn detects_standard_and_burst_observatory() {
        let standard: Value = json!({"observatory": {"subjectSelector": []}});
        let burst: Value = json!({"burstObservatory": {"subjectSelector": []}});
        let missing: Value = json!({"observatory": {"probeUrl": "https://example.com"}});

        assert!(has_observatory(&standard));
        assert!(has_observatory(&burst));
        assert!(!has_observatory(&missing));
    }
}
