use serde_json::{json, Value};
use std::collections::HashSet;
use std::error::Error;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::terminal::{info, read_input, success, warning};
use crate::xray_test::{test_config_set_detailed, XrayConfigInput};

type AppResult<T> = Result<T, Box<dyn Error>>;

const CONFIG_DIR: &str = "/opt/etc/xray/configs";
const PROGRAM_BALANCER_TAG: &str = "xkeen-sub-balancer";
const OBSERVATORY_KEYS: [&str; 3] = ["observatory", "burstObservatory", "burstobservatory"];

struct ConfigFile {
    path: PathBuf,
    name: String,
    original: String,
    document: Value,
}

pub fn run_install_config_check() -> AppResult<()> {
    let config_dir = Path::new(CONFIG_DIR);
    if !config_dir.is_dir() {
        return Err(format!("Каталог конфигурации Xray не найден: {}", config_dir.display()).into());
    }

    let mut configs = load_configs(config_dir)?;
    if configs.is_empty() {
        return Err("В каталоге Xray не найдены файлы JSON/JSONC для проверки.".into());
    }

    let mut changed = HashSet::<usize>::new();

    for config in &configs {
        if let Some(routing_value) = config.document.get("routing") {
            let routing = routing_value.as_object()
                .ok_or_else(|| format!("Поле routing в {} должно быть объектом.", config.name))?;
            if let Some(balancers) = routing.get("balancers") {
                if !balancers.is_array() {
                    return Err(format!("Поле routing.balancers в {} должно быть массивом.", config.name).into());
                }
            }
        }

    }

    // Detect xkeen-sub's own balancer templates in each routing file.
    let mut legacy_candidates = Vec::<usize>::new();
    let mut incomplete_advanced = Vec::<usize>::new();

    for (index, config) in configs.iter().enumerate() {
        let Some(balancers) = config.document.get("routing")
            .and_then(|routing| routing.get("balancers"))
            .and_then(Value::as_array) else { continue };

        match inspect_program_balancers(balancers) {
            ProgramBalancerState::Simple => success(&format!("{}: найден шаблон Simple.", config.name)),
            ProgramBalancerState::Advanced => success(&format!("{}: найден полный шаблон Advanced.", config.name)),
            ProgramBalancerState::IncompleteAdvanced => incomplete_advanced.push(index),
            ProgramBalancerState::Mixed => warning(&format!(
                "{}: найдены балансировщики xkeen-sub вместе с пользовательскими. Автоматическая замена не предлагается.",
                config.name
            )),
            ProgramBalancerState::Ambiguous => warning(&format!(
                "{}: структура балансировщиков xkeen-sub неоднозначна (повторяющиеся или некорректные Tag). Автоматическая замена пропущена.",
                config.name
            )),
            ProgramBalancerState::NoProgram => {
                if !balancers.is_empty() { legacy_candidates.push(index); }
            }
        }
    }

    if !incomplete_advanced.is_empty() {
        info("Найден неполный шаблон Advanced: основной балансировщик и только один из двух дополнительных.");
        for index in &incomplete_advanced {
            let tags = configs[*index].document["routing"]["balancers"].as_array().unwrap()
                .iter().filter_map(|item| item.get("tag").and_then(Value::as_str))
                .collect::<Vec<_>>().join(", ");
            info(&format!("  {}: {}", configs[*index].name, tags));
        }
        warning("Замена восстановит полный шаблон Advanced. Пользовательских балансировщиков в этих файлах не обнаружено.");
        if confirm("Заменить неполный шаблон актуальным шаблоном Advanced? (1 — да, 2 — нет): ")? {
            for index in &incomplete_advanced {
                let routing = configs[*index].document["routing"].as_object_mut()
                    .ok_or("Поле routing должно быть объектом.")?;
                routing.insert("balancers".to_owned(), advanced_balancers_template());
                changed.insert(*index);
            }
            success("Неполный шаблон заменён полным шаблоном Advanced.");
        } else {
            info("Неполный шаблон Advanced оставлен без изменений.");
        }
    }

    // Without the primary xkeen-sub tag, similarly named balancers are not
    // considered ours. Keep the existing opt-in replacement flow for them.
    if !legacy_candidates.is_empty() {
        info("В конфигурации найдены балансировщики без основного Tag xkeen-sub:");
        for index in &legacy_candidates {
            let balancers = configs[*index].document["routing"]["balancers"].as_array().unwrap();
            let tags = balancers.iter().filter_map(|item| item.get("tag").and_then(Value::as_str))
                .collect::<Vec<_>>().join(", ");
            info(&format!("  {}: {}", configs[*index].name, if tags.is_empty() { "без Tag".to_owned() } else { tags }));
        }
        warning("Замена удалит перечисленные балансировщики и оставит только xkeen-sub-balancer.");
        let selectors = available_outbound_tags(&configs);
        if selectors.is_empty() {
            warning("Не найдено ни одного подходящего исходящего подключения. Замена балансировщиков пропущена, чтобы не создавать пустой selector.");
        } else if confirm("Заменить пользовательские балансировщики? (1 — да, 2 — нет): ")? {
            for index in &legacy_candidates {
                let routing = configs[*index].document["routing"].as_object_mut()
                    .ok_or("Поле routing должно быть объектом.")?;
                routing.insert("balancers".to_owned(), Value::Array(vec![program_balancer(&selectors)]));
                changed.insert(*index);
            }

            for (index, config) in configs.iter_mut().enumerate() {
                let Some(routing) = config.document.get_mut("routing").and_then(Value::as_object_mut) else { continue };
                let Some(rules) = routing.get_mut("rules").and_then(Value::as_array_mut) else { continue };
                let mut modified = false;
                for rule in rules {
                    let Some(object) = rule.as_object_mut() else { continue };
                    if let Some(tag) = object.get_mut("balancerTag") {
                        if tag.as_str() != Some(PROGRAM_BALANCER_TAG) {
                            *tag = Value::String(PROGRAM_BALANCER_TAG.to_owned());
                            modified = true;
                        }
                    }
                }
                if modified { changed.insert(index); }
            }
            success("Балансировщики будут заменены на xkeen-sub-balancer.");
        } else {
            info("Пользовательские балансировщики оставлены без изменений.");
        }
    } else if incomplete_advanced.is_empty() {
        success("Балансировщики xkeen-sub не требуют восстановления.");
    }

    // Check every top-level Observatory variant in every JSON/JSONC file.
    let mut observatory_locations = Vec::<(usize, String)>::new();
    for (index, config) in configs.iter().enumerate() {
        if let Some(root) = config.document.as_object() {
            for key in OBSERVATORY_KEYS {
                if root.contains_key(key) {
                    observatory_locations.push((index, key.to_owned()));
                }
            }
        }
    }

    if observatory_locations.is_empty() {
        // Preserve the historical installer behavior: create our template if
        // no Observatory is configured at all, without asking a needless question.
        ensure_default_observatory(&mut configs, config_dir, &mut changed)?;
        success("Добавлен стандартный шаблон Observatory.");
    } else {
        let differs = observatory_locations.iter().any(|(index, key)| {
            !observatory_matches_template(&configs[*index].document[key])
        });
        if differs {
            info("Найдены настройки Observatory, отличающиеся от шаблона xkeen-sub:");
            for (index, key) in &observatory_locations {
                info(&format!("  {} ({key})", configs[*index].name));
            }
            warning("Замена удалит дополнительные и пользовательские параметры Observatory.");
            if confirm("Заменить настройки Observatory стандартным шаблоном xkeen-sub? (1 — да, 2 — нет): ")? {
                for (index, key) in &observatory_locations {
                    let root = configs[*index].document.as_object_mut()
                        .ok_or("Корень конфигурации должен быть объектом.")?;
                    root.insert(key.to_owned(), observatory_template());
                    changed.insert(*index);
                }
                success("Настройки Observatory заменены стандартным шаблоном.");
            } else {
                info("Пользовательские настройки Observatory оставлены без изменений.");
            }
        } else {
            success("Настройки Observatory уже соответствуют шаблону xkeen-sub.");
        }
    }

    if changed.is_empty() {
        success("Изменения конфигурации не требуются.");
        return Ok(());
    }

    // Validate the complete resulting config set before touching active files.
    let staged: Vec<XrayConfigInput> = configs.iter().enumerate()
        .map(|(index, config)| {
            let content = if changed.contains(&index) {
                serde_json::to_string_pretty(&config.document)
                    .expect("serde_json::Value serialization cannot fail")
            } else {
                config.original.clone()
            };
            XrayConfigInput::new(config.name.clone(), content)
        })
        .collect();

    info("Проверяем итоговую конфигурацию командой Xray...");
    test_config_set_detailed(&staged).map_err(|error| {
        format!("Итоговая конфигурация не прошла проверку Xray. Файлы не изменены. {error}")
    })?;

    persist_changes(&mut configs, &changed)?;
    success("Конфигурация успешно проверена и сохранена.");
    Ok(())
}

fn load_configs(config_dir: &Path) -> AppResult<Vec<ConfigFile>> {
    let mut paths = Vec::new();
    for entry in fs::read_dir(config_dir)? {
        let path = entry?.path();
        if path.is_file() && is_json_config(&path) {
            paths.push(path);
        }
    }
    paths.sort();

    let mut configs = Vec::new();
    for path in paths {
        let original = fs::read_to_string(&path)
            .map_err(|error| format!("Не удалось прочитать {}: {error}", path.display()))?;
        let document: Value = json5::from_str(&original)
            .map_err(|error| format!("Не удалось разобрать {}: {error}", path.display()))?;
        let name = path.file_name().and_then(|name| name.to_str())
            .ok_or("Не удалось определить имя конфигурационного файла.")?.to_owned();
        configs.push(ConfigFile { path, name, original, document });
    }
    Ok(configs)
}

fn is_json_config(path: &Path) -> bool {
    path.extension().and_then(|extension| extension.to_str())
        .map(|extension| extension.eq_ignore_ascii_case("json") || extension.eq_ignore_ascii_case("jsonc"))
        .unwrap_or(false)
}

fn confirm(prompt: &str) -> AppResult<bool> {
    loop {
        match read_input(prompt)?.as_str() {
            "1" => return Ok(true),
            "2" => return Ok(false),
            _ => warning("Введите 1 для подтверждения или 2 для отказа."),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProgramBalancerState { NoProgram, Simple, Advanced, IncompleteAdvanced, Mixed, Ambiguous }

const PROGRAM_BALANCER_TAGS: [&str; 3] = [
    PROGRAM_BALANCER_TAG,
    "xkeen-sub-balancer-rezerve",
    "xkeen-sub-rest",
];

fn inspect_program_balancers(balancers: &[Value]) -> ProgramBalancerState {
    let mut found = HashSet::<&str>::new();
    let mut has_user_balancer = false;
    let mut duplicate = false;
    for balancer in balancers {
        let Some(tag) = balancer.get("tag").and_then(Value::as_str) else {
            has_user_balancer = true;
            continue;
        };
        if PROGRAM_BALANCER_TAGS.contains(&tag) {
            if !found.insert(tag) { duplicate = true; }
        } else {
            has_user_balancer = true;
        }
    }
    if !found.contains(PROGRAM_BALANCER_TAG) { return ProgramBalancerState::NoProgram; }
    if duplicate { return ProgramBalancerState::Ambiguous; }
    if has_user_balancer { return ProgramBalancerState::Mixed; }
    match found.len() {
        1 => ProgramBalancerState::Simple,
        2 => ProgramBalancerState::IncompleteAdvanced,
        3 => ProgramBalancerState::Advanced,
        _ => ProgramBalancerState::Ambiguous,
    }
}

fn advanced_balancers_template() -> Value {
    json!([
        { "tag": "xkeen-sub-balancer", "fallbackTag": "loopback-out", "strategy": { "type": "leastPing" } },
        { "tag": "xkeen-sub-balancer-rezerve", "fallbackTag": "loopback-out-rezerve", "strategy": { "type": "random" } },
        { "tag": "xkeen-sub-rest", "fallbackTag": "direct", "strategy": { "type": "random" } }
    ])
}

fn program_balancer(selectors: &[String]) -> Value {
    json!({
        "tag": PROGRAM_BALANCER_TAG,
        "selector": selectors,
        "fallbackTag": "direct",
        "strategy": { "type": "leastPing" }
    })
}

fn available_outbound_tags(configs: &[ConfigFile]) -> Vec<String> {
    let protected = ["freedom", "blackhole", "dns", "loopback"];
    let mut tags = Vec::new();
    for config in configs {
        let Some(outbounds) = config.document.get("outbounds").and_then(Value::as_array) else {
            continue;
        };
        for outbound in outbounds {
            let Some(tag) = outbound.get("tag").and_then(Value::as_str) else {
                continue;
            };
            let protocol = outbound.get("protocol").and_then(Value::as_str).unwrap_or("");
            if protected.contains(&protocol) || tag == "api" || tag == PROGRAM_BALANCER_TAG {
                continue;
            }
            if !tags.iter().any(|existing: &String| existing.as_str() == tag) {
                tags.push(tag.to_owned());
            }
        }
    }
    tags
}

fn observatory_template() -> Value {
    json!({
        "subjectSelector": [],
        "probeUrl": "https://www.google.com/generate_204",
        "probeInterval": "60s",
        "enableConcurrency": true
    })
}

// The selector is populated later by xkeen-sub during subscription updates.
// Its contents therefore do not make an otherwise standard Observatory custom.
fn observatory_matches_template(value: &Value) -> bool {
    let Some(object) = value.as_object() else { return false };
    object.len() == 4
        && object.get("subjectSelector").and_then(Value::as_array).is_some()
        && object.get("probeUrl").and_then(Value::as_str) == Some("https://www.google.com/generate_204")
        && object.get("probeInterval").and_then(Value::as_str) == Some("60s")
        && object.get("enableConcurrency").and_then(Value::as_bool) == Some(true)
}

fn ensure_default_observatory(
    configs: &mut Vec<ConfigFile>,
    config_dir: &Path,
    changed: &mut HashSet<usize>,
) -> AppResult<()> {
    if let Some(index) = configs.iter().position(|config| config.name == "observatory.json") {
        let root = configs[index].document.as_object_mut()
            .ok_or("Корень observatory.json должен быть объектом.")?;
        root.insert("observatory".to_owned(), observatory_template());
        changed.insert(index);
    } else {
        let path = config_dir.join("observatory.json");
        configs.push(ConfigFile {
            path,
            name: "observatory.json".to_owned(),
            original: String::new(),
            document: json!({ "observatory": observatory_template() }),
        });
        changed.insert(configs.len() - 1);
    }
    Ok(())
}

fn persist_changes(configs: &mut [ConfigFile], changed: &HashSet<usize>) -> AppResult<()> {
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let mut backups = Vec::<(usize, PathBuf, bool)>::new();
    let mut written = Vec::<usize>::new();

    // Create all backups before writing any active configuration.
    for index in changed {
        let config = &configs[*index];
        let existed = config.path.exists();
        let backup = PathBuf::from(format!("{}.xkeen-sub-install-{nonce}.bak", config.path.display()));
        if existed {
            fs::copy(&config.path, &backup)
                .map_err(|error| format!("Не удалось создать резервную копию {}: {error}", config.path.display()))?;
        }
        backups.push((*index, backup, existed));
    }

    for index in changed {
        let config = &configs[*index];
        let content = serde_json::to_string_pretty(&config.document)?;
        let temp = config.path.with_file_name(format!(
            ".{}.xkeen-sub-{}.tmp",
            config.name,
            std::process::id()
        ));
        let result = (|| -> AppResult<()> {
            let mut file = OpenOptions::new().write(true).create_new(true).mode(0o644).open(&temp)?;
            file.write_all(content.as_bytes())?;
            file.sync_all()?;
            fs::rename(&temp, &config.path)?;
            Ok(())
        })();

        if let Err(error) = result {
            let _ = fs::remove_file(&temp);
            for written_index in written.iter().rev() {
                if let Some((_, backup, existed)) = backups.iter().find(|(i, _, _)| i == written_index) {
                    if *existed {
                        let _ = fs::copy(backup, &configs[*written_index].path);
                    } else {
                        let _ = fs::remove_file(&configs[*written_index].path);
                    }
                }
            }
            return Err(format!("Не удалось сохранить {}: {error}. Предыдущие файлы восстановлены.", config.path.display()).into());
        }
        written.push(*index);
    }

    for (_, backup, existed) in backups {
        if existed {
            info(&format!("Резервная копия: {}", backup.display()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{observatory_matches_template, observatory_template, program_balancer, PROGRAM_BALANCER_TAG};
    use serde_json::json;

    fn inspect(value: serde_json::Value) -> super::ProgramBalancerState {
        super::inspect_program_balancers(value.as_array().expect("balancers must be an array"))
    }

    #[test]
    fn detects_simple_template_by_primary_tag_only() {
        assert_eq!(inspect(json!([{ "tag": "xkeen-sub-balancer" }])), super::ProgramBalancerState::Simple);
    }

    #[test]
    fn detects_complete_advanced_template() {
        assert_eq!(inspect(json!([
            { "tag": "xkeen-sub-balancer" },
            { "tag": "xkeen-sub-balancer-rezerve" },
            { "tag": "xkeen-sub-rest" }
        ])), super::ProgramBalancerState::Advanced);
    }

    #[test]
    fn detects_incomplete_advanced_only_without_user_balancers() {
        assert_eq!(inspect(json!([
            { "tag": "xkeen-sub-balancer" }, { "tag": "xkeen-sub-rest" }
        ])), super::ProgramBalancerState::IncompleteAdvanced);
        assert_eq!(inspect(json!([
            { "tag": "xkeen-sub-balancer" }, { "tag": "xkeen-sub-rest" }, { "tag": "my-user-balancer" }
        ])), super::ProgramBalancerState::Mixed);
    }

    #[test]
    fn advanced_tags_without_primary_are_not_recognized_as_program_template() {
        assert_eq!(inspect(json!([
            { "tag": "xkeen-sub-balancer-rezerve" }, { "tag": "xkeen-sub-rest" }
        ])), super::ProgramBalancerState::NoProgram);
    }

    #[test]
    fn advanced_template_has_no_selectors_and_expected_fallbacks() {
        let template = super::advanced_balancers_template();
        assert_eq!(template.as_array().unwrap().len(), 3);
        assert!(template.as_array().unwrap().iter().all(|item| item.get("selector").is_none()));
        assert_eq!(template[0]["fallbackTag"], "loopback-out");
        assert_eq!(template[0]["strategy"]["type"], "leastPing");
        assert_eq!(template[1]["fallbackTag"], "loopback-out-rezerve");
        assert_eq!(template[1]["strategy"]["type"], "random");
        assert_eq!(template[2]["fallbackTag"], "direct");
        assert_eq!(template[2]["strategy"]["type"], "random");
    }

    #[test]
    fn program_balancer_uses_expected_tag_and_strategy() {
        let balancer = program_balancer(&["sub1--node".to_owned()]);
        assert_eq!(balancer["tag"], PROGRAM_BALANCER_TAG);
        assert_eq!(balancer["selector"], json!(["sub1--node"]));
        assert_eq!(balancer["strategy"]["type"], "leastPing");
    }

    #[test]
    fn standard_observatory_allows_runtime_selector_values() {
        let mut observatory = observatory_template();
        observatory["subjectSelector"] = json!(["sub1--node"]);
        assert!(observatory_matches_template(&observatory));
    }

    #[test]
    fn custom_observatory_is_detected() {
        let custom = json!({
            "subjectSelector": [],
            "probeUrl": "https://example.com",
            "probeInterval": "60s",
            "enableConcurrency": true
        });
        assert!(!observatory_matches_template(&custom));
    }

    #[test]
    fn extra_observatory_options_are_detected_as_custom() {
        let mut custom = observatory_template();
        custom["extra"] = json!(true);
        assert!(!observatory_matches_template(&custom));
    }
}
