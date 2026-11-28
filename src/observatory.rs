use serde_json::{json, Value};
use std::error::Error;

type AppResult<T> = Result<T, Box<dyn Error>>;

const OBSERVATORY_KEYS: [&str; 3] = [
    "observatory",
    "burstObservatory",
    "burstobservatory",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservatoryUpdate {
    pub file_name: String,
    pub object_name: String,
    pub created: bool,
}

/// Synchronizes existing Observatory selectors. If no Observatory object is
/// configured, prepares the default observatory.json document in memory.
pub fn prepare_observatory(
    documents: &mut Vec<(String, Value)>,
    tags: &[String],
) -> AppResult<Vec<ObservatoryUpdate>> {
    prepare_observatory_with_interval(documents, tags, None)
}

/// Synchronizes selectors and, when provided, applies the interval to existing and new Observatory objects.
pub fn prepare_observatory_with_interval(
    documents: &mut Vec<(String, Value)>,
    tags: &[String],
    probe_interval: Option<&str>,
) -> AppResult<Vec<ObservatoryUpdate>> {
    let selector = Value::Array(tags.iter().cloned().map(Value::String).collect());
    let mut updated = Vec::new();

    for (file_name, document) in documents.iter_mut() {
        let Some(root) = document.as_object_mut() else {
            continue;
        };

        for key in OBSERVATORY_KEYS {
            let Some(value) = root.get_mut(key) else {
                continue;
            };
            let object = value.as_object_mut().ok_or_else(|| {
                format!("Объект {key} в файле {file_name} должен быть JSON-объектом.")
            })?;

            object.insert("subjectSelector".to_owned(), selector.clone());
            if let Some(interval) = probe_interval {
                object.insert("probeInterval".to_owned(), Value::String(interval.to_owned()));
            }
            updated.push(ObservatoryUpdate {
                file_name: file_name.clone(),
                object_name: key.to_owned(),
                created: false,
            });
        }
    }

    if !updated.is_empty() {
        return Ok(updated);
    }

    let observatory = json!({
        "subjectSelector": selector,
        "probeUrl": "https://www.google.com/generate_204",
        "probeInterval": probe_interval.unwrap_or("60s"),
        "enableConcurrency": true
    });

    if let Some((file_name, document)) = documents
        .iter_mut()
        .find(|(file_name, _)| file_name.as_str() == "observatory.json")
    {
        let root = document.as_object_mut().ok_or_else(|| {
            "Корень файла observatory.json должен быть JSON-объектом.".to_owned()
        })?;
        root.insert("observatory".to_owned(), observatory);
        updated.push(ObservatoryUpdate {
            file_name: file_name.clone(),
            object_name: "observatory".to_owned(),
            created: true,
        });
    } else {
        documents.push((
            "observatory.json".to_owned(),
            json!({ "observatory": observatory }),
        ));
        updated.push(ObservatoryUpdate {
            file_name: "observatory.json".to_owned(),
            object_name: "observatory".to_owned(),
            created: true,
        });
    }

    Ok(updated)
}

#[cfg(test)]
mod tests {
    use super::prepare_observatory;
    use serde_json::json;

    #[test]
    fn updates_existing_observatory_and_preserves_other_fields() {
        let mut docs = vec![(
            "observatory.json".to_owned(),
            json!({
                "observatory": {
                    "subjectSelector": ["old"],
                    "probeUrl": "https://example.com",
                    "probeInterval": "30s"
                }
            }),
        )];

        let updates = prepare_observatory(
            &mut docs,
            &["sub1--node".to_owned(), "custom".to_owned()],
        )
        .unwrap();

        assert_eq!(updates.len(), 1);
        assert!(!updates[0].created);
        assert_eq!(
            docs[0].1["observatory"]["subjectSelector"],
            json!(["sub1--node", "custom"])
        );
        assert_eq!(
            docs[0].1["observatory"]["probeUrl"],
            "https://example.com"
        );
        assert_eq!(docs[0].1["observatory"]["probeInterval"], "30s");
    }

    #[test]
    fn updates_burst_observatory_and_adds_missing_selector() {
        let mut docs = vec![(
            "custom.json".to_owned(),
            json!({"burstObservatory": {"probeInterval": "1m"}}),
        )];

        prepare_observatory(&mut docs, &["node".to_owned()]).unwrap();

        assert_eq!(
            docs[0].1["burstObservatory"]["subjectSelector"],
            json!(["node"])
        );
        assert!(docs[0].1.get("observatory").is_none());
    }

    #[test]
    fn updates_multiple_observatory_objects() {
        let mut docs = vec![
            (
                "one.json".to_owned(),
                json!({"observatory": {"subjectSelector": []}}),
            ),
            (
                "two.json".to_owned(),
                json!({"burstObservatory": {"subjectSelector": []}}),
            ),
        ];

        let updates = prepare_observatory(&mut docs, &["node".to_owned()]).unwrap();

        assert_eq!(updates.len(), 2);
        assert!(updates.iter().all(|update| !update.created));
        assert_eq!(docs[0].1["observatory"]["subjectSelector"], json!(["node"]));
        assert_eq!(docs[1].1["burstObservatory"]["subjectSelector"], json!(["node"]));
    }

    #[test]
    fn creates_default_observatory_file_when_no_object_exists() {
        let mut docs = vec![("routing.json".to_owned(), json!({"routing": {}}))];

        let updates = prepare_observatory(&mut docs, &["sub1--node".to_owned()]).unwrap();

        assert_eq!(docs.len(), 2);
        assert_eq!(updates.len(), 1);
        assert!(updates[0].created);
        assert_eq!(updates[0].file_name, "observatory.json");
        assert_eq!(
            docs[1].1,
            json!({
                "observatory": {
                    "subjectSelector": ["sub1--node"],
                    "probeUrl": "https://www.google.com/generate_204",
                    "probeInterval": "60s",
                    "enableConcurrency": true
                }
            })
        );
    }

    #[test]
    fn adds_default_observatory_to_existing_file_without_overwriting_other_fields() {
        let mut docs = vec![(
            "observatory.json".to_owned(),
            json!({"log": {"loglevel": "warning"}}),
        )];

        prepare_observatory(&mut docs, &[]).unwrap();

        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].1["log"]["loglevel"], "warning");
        assert_eq!(docs[0].1["observatory"]["subjectSelector"], json!([]));
    }


    #[test]
    fn supplied_interval_overrides_existing_and_new_observatory() {
        let mut existing = vec![(
            "observatory.json".to_owned(),
            json!({"observatory": {"probeInterval": "60s"}}),
        )];
        super::prepare_observatory_with_interval(&mut existing, &["node".to_owned()], Some("10s")).unwrap();
        assert_eq!(existing[0].1["observatory"]["probeInterval"], "10s");

        let mut missing = vec![("routing.json".to_owned(), json!({"routing": {}}))];
        super::prepare_observatory_with_interval(&mut missing, &["node".to_owned()], Some("10s")).unwrap();
        assert_eq!(missing[1].1["observatory"]["probeInterval"], "10s");

        let mut simple = vec![(
            "observatory.json".to_owned(),
            json!({"observatory": {"probeInterval": "10s"}}),
        )];
        super::prepare_observatory_with_interval(&mut simple, &["node".to_owned()], Some("60s")).unwrap();
        assert_eq!(simple[0].1["observatory"]["probeInterval"], "60s");
    }

    #[test]
    fn no_interval_preserves_user_configured_value() {
        let mut docs = vec![(
            "observatory.json".to_owned(),
            json!({"observatory": {"probeInterval": "17s"}}),
        )];
        super::prepare_observatory_with_interval(&mut docs, &["node".to_owned()], None).unwrap();
        assert_eq!(docs[0].1["observatory"]["probeInterval"], "17s");
    }

    #[test]
    fn malformed_existing_observatory_is_an_error() {
        let mut docs = vec![("bad.json".to_owned(), json!({"observatory": []}))];
        assert!(prepare_observatory(&mut docs, &[]).is_err());
    }
}
