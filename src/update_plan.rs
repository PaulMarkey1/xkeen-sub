use std::collections::BTreeMap;
use std::error::Error;
use std::fs;
use std::path::PathBuf;

use serde_json::Value;

use crate::update_state::{PreparedBaseline, RunState};
use crate::update_config::ConfigDocument;
use crate::update_apply::{PlannedFileOperation, PlannedOperationKind};
use crate::update_merge::{ArchivePlan, USER_ARCHIVE};
use crate::subscriptions::{SubscriptionStore, SUBSCRIPTIONS_FILE};

type AppResult<T> = Result<T, Box<dyn Error>>;

pub(crate) fn build_file_plan(
    docs: &[ConfigDocument],
    state: &RunState,
    archive: &ArchivePlan,
) -> AppResult<Vec<PlannedFileOperation>> {
    let mut plan: BTreeMap<PathBuf, PlannedFileOperation> = BTreeMap::new();

    for doc in docs {
        // Keep untouched JSON/JSON5 files byte-for-byte as they were read.
        // Re-serialize only documents whose parsed configuration was changed.
        if doc.value != doc.original_value {
            let mut content = serde_json::to_vec_pretty(&doc.value)?;
            content.push(b'\n');
            plan.insert(doc.path.clone(), PlannedFileOperation {
                path: doc.path.clone(),
                category: "Конфигурация Xray",
                kind: PlannedOperationKind::Replace,
                content: Some(content),
            });
        }
    }

    if archive.added > 0 {
        let path = PathBuf::from(USER_ARCHIVE);
        let mut content = serde_json::to_vec_pretty(&archive.outbounds)?;
        content.push(b'\n');
        match fs::read(&path) {
            Ok(original) if original == content => {}
            Ok(_) => {
                plan.insert(path.clone(), PlannedFileOperation {
                    path,
                    category: "Архив пользовательских подключений",
                    kind: PlannedOperationKind::Replace,
                    content: Some(content),
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                plan.insert(path.clone(), PlannedFileOperation {
                    path,
                    category: "Архив пользовательских подключений",
                    kind: PlannedOperationKind::Create,
                    content: Some(content),
                });
            }
            Err(error) => return Err(format!("Не удалось проверить архив {}: {error}", path.display()).into()),
        }
    }

    for baseline in &state.baseline_updates {
        match fs::read(&baseline.path) {
            Ok(original) if original == baseline.original => {}
            Ok(_) => {
                plan.insert(baseline.path.clone(), PlannedFileOperation {
                    path: baseline.path.clone(),
                    category: "Baseline",
                    kind: PlannedOperationKind::Replace,
                    content: Some(baseline.original.clone()),
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                plan.insert(baseline.path.clone(), PlannedFileOperation {
                    path: baseline.path.clone(),
                    category: "Baseline",
                    kind: PlannedOperationKind::Create,
                    content: Some(baseline.original.clone()),
                });
            }
            Err(error) => return Err(format!("Не удалось проверить baseline {}: {error}", baseline.path.display()).into()),
        }
    }

    for (id, path) in &state.baseline_removals {
        if plan.contains_key(path) {
            // A replacement at the same path supersedes a separate removal.
            continue;
        }
        match fs::metadata(path) {
            Ok(metadata) if metadata.is_file() => {
                plan.insert(path.clone(), PlannedFileOperation {
                    path: path.clone(),
                    category: "Повреждённый baseline",
                    kind: PlannedOperationKind::Delete,
                    content: None,
                });
            }
            Ok(_) => return Err(format!("Путь baseline для {id} не является обычным файлом: {}", path.display()).into()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("Не удалось проверить baseline для удаления {}: {error}", path.display()).into()),
        }
    }

    Ok(plan.into_values().collect())
}

/// Extends the normal update plan with an atomic subscription registry replacement.
pub(crate) fn build_file_plan_with_registry(
    docs: &[ConfigDocument],
    state: &RunState,
    archive: &ArchivePlan,
    store: &SubscriptionStore,
) -> AppResult<Vec<PlannedFileOperation>> {
    let mut plan = build_file_plan(docs, state, archive)?;
    let path = PathBuf::from(SUBSCRIPTIONS_FILE);
    let mut content = serde_json::to_vec_pretty(store)?;
    content.push(b'\n');

    match fs::read(&path) {
        Ok(original) if original == content => {}
        Ok(_) => plan.push(PlannedFileOperation {
            path,
            category: "Реестр подписок",
            kind: PlannedOperationKind::Replace,
            content: Some(content),
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            plan.push(PlannedFileOperation {
                path,
                category: "Реестр подписок",
                kind: PlannedOperationKind::Create,
                content: Some(content),
            });
        }
        Err(error) => return Err(format!("Не удалось прочитать реестр подписок: {error}").into()),
    }

    plan.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(plan)
}

#[cfg(test)]
mod file_plan_tests {
    use super::{build_file_plan, ConfigDocument, PlannedOperationKind, PreparedBaseline, RunState};
    use crate::update_merge::ArchivePlan;
    use serde_json::json;
    use std::fs;
    use std::path::PathBuf;

    fn temporary_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("xkeen-sub-{label}-{}", std::process::id()))
    }

    #[test]
    fn byte_identical_prepared_config_and_baseline_are_excluded() {
        let root = temporary_path("plan-identical");
        fs::create_dir_all(&root).unwrap();
        let baseline_path = root.join("baseline.json");
        let baseline = b"{\"subscriptions\":[]}".to_vec();
        fs::write(&baseline_path, &baseline).unwrap();

        let value = json!({"log":{"loglevel":"warning"}});
        let mut original = serde_json::to_vec_pretty(&value).unwrap();
        original.push(b'\n');
        let doc = ConfigDocument {
            file_name: "config.json".to_owned(),
            path: root.join("config.json"),
            original: String::from_utf8(original).unwrap(),
            original_value: value.clone(),
            value,
        };
        let state = RunState {
            baseline_updates: vec![PreparedBaseline {
                id: "sub1".to_owned(),
                original: baseline,
                path: baseline_path,
            }],
            ..RunState::default()
        };
        let archive = ArchivePlan { outbounds: Vec::new(), added: 0 };

        let plan = build_file_plan(&[doc], &state, &archive).unwrap();
        assert!(plan.is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unchanged_parsed_config_is_not_rewritten_when_source_format_differs() {
        let root = temporary_path("plan-semantic-equality");
        fs::create_dir_all(&root).unwrap();

        let original = "{\n  // Keep this comment and formatting.\n  \"log\": { \"loglevel\": \"warning\" }\n}\n";
        let value = serde_json::from_str::<serde_json::Value>(
            r#"{ "log": { "loglevel": "warning" } }"#,
        ).unwrap();
        let doc = ConfigDocument {
            file_name: "01_log.json".to_owned(),
            path: root.join("01_log.json"),
            original: original.to_owned(),
            original_value: value.clone(),
            value,
        };
        let state = RunState::default();
        let archive = ArchivePlan { outbounds: Vec::new(), added: 0 };

        let plan = build_file_plan(&[doc], &state, &archive).unwrap();
        assert!(plan.is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn changed_parsed_config_is_included_in_file_plan() {
        let root = temporary_path("plan-semantic-change");
        fs::create_dir_all(&root).unwrap();

        let original_value = json!({"log":{"loglevel":"warning"}});
        let current_value = json!({"log":{"loglevel":"debug"}});
        let doc = ConfigDocument {
            file_name: "01_log.json".to_owned(),
            path: root.join("01_log.json"),
            original: r#"{ "log": { "loglevel": "warning" } }"#.to_owned(),
            original_value,
            value: current_value,
        };
        let state = RunState::default();
        let archive = ArchivePlan { outbounds: Vec::new(), added: 0 };

        let plan = build_file_plan(&[doc], &state, &archive).unwrap();
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].kind, PlannedOperationKind::Replace);
        assert_eq!(plan[0].path, root.join("01_log.json"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn baseline_replacement_supersedes_removal_at_the_same_path() {
        let root = temporary_path("plan-baseline-priority");
        fs::create_dir_all(&root).unwrap();
        let baseline_path = root.join("baseline.json");
        fs::write(&baseline_path, b"old baseline").unwrap();

        let state = RunState {
            baseline_updates: vec![PreparedBaseline {
                id: "sub1".to_owned(),
                original: b"new baseline".to_vec(),
                path: baseline_path.clone(),
            }],
            baseline_removals: vec![("sub1".to_owned(), baseline_path.clone())],
            ..RunState::default()
        };
        let archive = ArchivePlan { outbounds: Vec::new(), added: 0 };

        let plan = build_file_plan(&[], &state, &archive).unwrap();
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].path, baseline_path);
        assert_eq!(plan[0].kind, PlannedOperationKind::Replace);
        fs::remove_dir_all(root).unwrap();
    }
}

