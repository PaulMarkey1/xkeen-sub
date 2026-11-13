use serde_json::Value;
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
}

/// Updates subjectSelector in every top-level Observatory object found in the
/// supplied in-memory configuration documents. No filesystem access occurs.
pub fn synchronize_subject_selectors(
    documents: &mut [(String, Value)],
    tags: &[String],
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
            updated.push(ObservatoryUpdate {
                file_name: file_name.clone(),
                object_name: key.to_owned(),
            });
        }
    }

    if updated.is_empty() {
        return Err(
            "В конфигурационных файлах Xray не найден объект Observatory или Burst Observatory."
                .into(),
        );
    }

    Ok(updated)
}

#[cfg(test)]
mod tests {
    use super::synchronize_subject_selectors;
    use serde_json::json;

    #[test]
    fn updates_standard_observatory_and_preserves_other_fields() {
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

        let updates = synchronize_subject_selectors(
            &mut docs,
            &["sub1--node".to_owned(), "custom".to_owned()],
        )
        .unwrap();

        assert_eq!(updates.len(), 1);
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

        synchronize_subject_selectors(&mut docs, &["node".to_owned()]).unwrap();

        assert_eq!(
            docs[0].1["burstObservatory"]["subjectSelector"],
            json!(["node"])
        );
    }

    #[test]
    fn updates_multiple_observatory_objects() {
        let mut docs = vec![(
            "all.json".to_owned(),
            json!({
                "observatory": {"subjectSelector": []},
                "burstObservatory": {"subjectSelector": []}
            }),
        )];

        let updates = synchronize_subject_selectors(&mut docs, &[]).unwrap();

        assert_eq!(updates.len(), 2);
        assert_eq!(docs[0].1["observatory"]["subjectSelector"], json!([]));
        assert_eq!(docs[0].1["burstObservatory"]["subjectSelector"], json!([]));
    }

    #[test]
    fn missing_observatory_is_an_error() {
        let mut docs = vec![("routing.json".to_owned(), json!({"routing": {}}))];
        assert!(synchronize_subject_selectors(&mut docs, &[]).is_err());
    }
}
