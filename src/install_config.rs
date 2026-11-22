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
const LOOPBACK_RULE_PRIMARY = "loopback-in";
const LOOPBACK_RULE_RESERVE = "loopback-in-rezerve";

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

    // Detect xkeen-sub templates and user-defined balancers.
    let mut user_balancer_candidates = Vec::<usize>::new();
    let mut incomplete_advanced = Vec::<usize>::new();
    let mut simple_configs = Vec::<usize>::new();
    let mut advanced_configs = Vec::<usize>::new();

    for (index, config) in configs.iter().enumerate() {
        let Some(balancers) = config.document.get("routing")
            .and_then(|routing| routing.get("balancers"))
            .and_then(Value::as_array) else { continue };

        if balancers.is_empty() { continue; }
        match inspect_program_balancers(balancers) {
            ProgramBalancerState::Simple => {
                success(&format!("{}: найден шаблон Simple.", config.name));
                simple_configs.push(index);
            }
            ProgramBalancerState::Advanced => {
                success(&format!("{}: найден полный шаблон Advanced.", config.name));
                advanced_configs.push(index);
            }
            ProgramBalancerState::IncompleteAdvanced => incomplete_advanced.push(index),
            ProgramBalancerState::Mixed => user_balancer_candidates.push(index),
            ProgramBalancerState::Ambiguous => warning(&format!(
                "{}: структура балансировщиков xkeen-sub неоднозначна (повторяющиеся Tag). Автоматическая замена пропущена.",
                config.name
            )),
            ProgramBalancerState::NoProgram => user_balancer_candidates.push(index),
        }
    }

    let mut no_persistent_backup = HashSet::<usize>::new();

    // If there are no balancers at all, ask which xkeen-sub template to install.
    let any_nonempty_balancers = configs.iter().any(|config| {
        config.document.get("routing")
            .and_then(|routing| routing.get("balancers"))
            .and_then(Value::as_array)
            .map(|items| !items.is_empty())
            .unwrap_or(false)
    });

    if !any_nonempty_balancers {
        info("Балансировщики Xray не найдены.");
        info("  1 — установить простой режим");
        info("  2 — установить продвинутый режим");
        let mode = read_choice("Выберите режим: ", &["1", "2"])?;
        let target = configs.iter().position(|config| {
            config.document.get("routing").and_then(|routing| routing.get("rules"))
                .and_then(Value::as_array).is_some()
        }).or_else(|| configs.iter().position(|config| {
            config.document.get("routing").and_then(Value::as_object).is_some()
        })).ok_or("Не найден файл с объектом routing. Невозможно установить балансировщик.")?;

        let template = if mode == "1" {
            let selectors = available_outbound_tags(&configs);
            if selectors.is_empty() {
                return Err("Не найдено подходящих исходящих подключений. Балансировщик не установлен, чтобы не создавать пустой selector.".into());
            }
            Value::Array(vec![program_balancer(&selectors)])
        } else {
            advanced_balancers_template()
        };

        let routing = configs[target].document["routing"].as_object_mut()
            .ok_or("Поле routing должно быть объектом.")?;
        routing.insert("balancers".to_owned(), template);
        changed.insert(target);
        let mode_kind = if mode == "2" { RoutingMode::Advanced } else { RoutingMode::Simple };
        apply_routing_mode(&mut configs, mode_kind, Some(target), &mut changed)?;
        success(if mode == "1" {
            "Установлен шаблон Simple."
        } else {
            "Установлен шаблон Advanced."
        });
    } else {
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
                apply_routing_mode(&mut configs, RoutingMode::Advanced, incomplete_advanced.first().copied(), &mut changed)?;
                success("Шаблон Advanced восстановлен, правила маршрутизации проверены.");
            } else {
                info("Неполный шаблон Advanced оставлен без изменений.");
            }
        }

        // An already installed template is also a declaration of the selected mode.
        // Keep its routing rules synchronized on every installer check.
        if !simple_configs.is_empty() {
            apply_routing_mode(&mut configs, RoutingMode::Simple, simple_configs.first().copied(), &mut changed)?;
        }
        if !advanced_configs.is_empty() {
            apply_routing_mode(&mut configs, RoutingMode::Advanced, advanced_configs.first().copied(), &mut changed)?;
        }

        if !user_balancer_candidates.is_empty() {
            info("Найдены пользовательские балансировщики:");
            for index in &user_balancer_candidates {
                let balancers = configs[*index].document["routing"]["balancers"].as_array().unwrap();
                let tags = balancers.iter().filter_map(|item| item.get("tag").and_then(Value::as_str))
                    .collect::<Vec<_>>().join(", ");
                info(&format!("  {}: {}", configs[*index].name, if tags.is_empty() { "без Tag".to_owned() } else { tags }));
            }

            info("Как поступить с найденными балансировщиками?");
            info("  1 — заменить с резервной копией в /opt");
            info("  2 — заменить без резервной копии");
            info("  3 — оставить пользовательские и добавить только основной балансировщик xkeen-sub");
            let replacement_choice = read_choice("Ваш выбор: ", &["1", "2", "3"])?;

            if replacement_choice == "1" || replacement_choice == "2" {
                info("Выберите шаблон балансировщиков:");
                info("  1 — Simple");
                info("  2 — Advanced");
                let mode = read_choice("Ваш выбор: ", &["1", "2"])?;
                let replacement = if mode == "1" {
                    let selectors = available_outbound_tags(&configs);
                    if selectors.is_empty() {
                        warning("Не найдено подходящих исходящих подключений. Замена отменена, чтобы не создавать пустой selector.");
                        None
                    } else {
                        Some(Value::Array(vec![program_balancer(&selectors)]))
                    }
                } else {
                    Some(advanced_balancers_template())
                };

                if let Some(replacement) = replacement {
                    if replacement_choice == "1" {
                        create_opt_backups(&configs, &user_balancer_candidates)?;
                    }
                    for index in &user_balancer_candidates {
                        let routing = configs[*index].document["routing"].as_object_mut()
                            .ok_or("Поле routing должно быть объектом.")?;
                        routing.insert("balancers".to_owned(), replacement.clone());
                        changed.insert(*index);
                        no_persistent_backup.insert(*index);
                    }
                    let mode_kind = if mode == "2" { RoutingMode::Advanced } else { RoutingMode::Simple };
                    apply_routing_mode(&mut configs, mode_kind, user_balancer_candidates.first().copied(), &mut changed)?;
                    success(if mode == "1" {
                        "Установлен шаблон Simple, пользовательские правила перенаправлены на основной балансировщик."
                    } else {
                        "Установлен шаблон Advanced, пользовательские правила перенаправлены на основной балансировщик."
                    });
                }
            } else {
                // Preserve the user's routing exactly. Add only our own primary
                // balancer so it can be selected manually by the user later.
                let selectors = available_outbound_tags(&configs);
                if selectors.is_empty() {
                    warning("Не найдено подходящих исходящих подключений. Основной балансировщик не добавлен.");
                } else {
                    for index in &user_balancer_candidates {
                        let routing = configs[*index].document["routing"].as_object_mut()
                            .ok_or("Поле routing должно быть объектом.")?;
                        let balancers = routing.entry("balancers").or_insert_with(|| Value::Array(Vec::new()))
                            .as_array_mut().ok_or("Поле routing.balancers должно быть массивом.")?;
                        if !balancers.iter().any(|item| item.get("tag").and_then(Value::as_str) == Some(PROGRAM_BALANCER_TAG)) {
                            balancers.push(program_balancer(&selectors));
                            changed.insert(*index);
                        }
                    }
                    success("Пользовательские балансировщики и правила маршрутизации оставлены без изменений. Добавлен только основной балансировщик xkeen-sub.");
                }
            }
        }
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

    persist_changes(&mut configs, &changed, &no_persistent_backup)?;
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

fn read_choice(prompt: &str, allowed: &[&str]) -> AppResult<String> {
    loop {
        let value = read_input(prompt)?;
        if allowed.contains(&value.as_str()) {
            return Ok(value);
        }
        warning("Выберите один из предложенных вариантов.");
    }
}

fn create_opt_backups(configs: &[ConfigFile], indices: &[usize]) -> AppResult<()> {
    let backup_dir = Path::new("/opt");
    if !backup_dir.is_dir() {
        return Err("Каталог /opt не найден. Резервная копия не создана, конфигурация не изменена.".into());
    }

    let mut targets = Vec::new();
    for index in indices {
        let config = &configs[*index];
        let backup_path = backup_dir.join(&config.name);
        if backup_path.exists() {
            return Err(format!(
                "Файл резервной копии уже существует: {}. Он не будет перезаписан. Удалите или переместите его и повторите установку.",
                backup_path.display()
            ).into());
        }
        targets.push((*index, backup_path));
    }

    let mut created = Vec::<PathBuf>::new();
    for (index, backup_path) in targets {
        if let Err(error) = fs::copy(&configs[index].path, &backup_path) {
            for path in created.iter().rev() {
                let _ = fs::remove_file(path);
            }
            return Err(format!(
                "Не удалось создать резервную копию {}: {error}. Созданные копии удалены, конфигурация не изменена.",
                backup_path.display()
            ).into());
        }
        created.push(backup_path.clone());
        success(&format!("Резервная копия создана: {}", backup_path.display()));
    }
    Ok(())
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
    if has_user_balancer { return ProgramBalancerState::Mixed; }
    if duplicate { return ProgramBalancerState::Ambiguous; }
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


#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RoutingMode {
    Simple,
    Advanced,
}

const PROTECTED_OUTBOUND_PROTOCOLS: [&str; 4] = ["freedom", "blackhole", "dns", "loopback"];

fn apply_routing_mode(
    configs: &mut [ConfigFile],
    mode: RoutingMode,
    loopback_owner: Option<usize>,
    changed: &mut HashSet<usize>,
) -> AppResult<()> {
    let protocols: std::collections::HashMap<String, String> = configs.iter()
        .filter_map(|config| config.document.get("outbounds").and_then(Value::as_array))
        .flatten()
        .filter_map(|outbound| Some((
            outbound.get("tag")?.as_str()?.to_owned(),
            outbound.get("protocol")?.as_str()?.to_owned(),
        )))
        .collect();

    for (index, config) in configs.iter_mut().enumerate() {
        let config_name = config.name.clone();
        let Some(routing) = config.document.get_mut("routing").and_then(Value::as_object_mut) else {
            continue;
        };
        let mut modified = false;
        if let Some(rules_value) = routing.get_mut("rules") {
            let rules = rules_value.as_array_mut()
                .ok_or_else(|| format!("Поле routing.rules в {} должно быть массивом.", config_name))?;
            for rule in rules.iter_mut() {
                let Some(object) = rule.as_object_mut() else { continue };
                if is_loopback_rule(object) {
                    continue;
                }

                if object.get("balancerTag").is_some() {
                    if object.get("balancerTag").and_then(Value::as_str) != Some(PROGRAM_BALANCER_TAG) {
                        object.insert("balancerTag".to_owned(), Value::String(PROGRAM_BALANCER_TAG.to_owned()));
                        modified = true;
                    }
                    modified |= object.remove("outboundTag").is_some();
                    continue;
                }

                let Some(outbound_tag) = object.get("outboundTag").and_then(Value::as_str).map(str::to_owned) else {
                    continue;
                };
                if ["api", "direct", "block"].contains(&outbound_tag.as_str()) {
                    continue;
                }
                if protocols.get(&outbound_tag)
                    .map(|protocol| PROTECTED_OUTBOUND_PROTOCOLS.contains(&protocol.as_str()))
                    .unwrap_or(false)
                {
                    continue;
                }
                object.remove("outboundTag");
                object.insert("balancerTag".to_owned(), Value::String(PROGRAM_BALANCER_TAG.to_owned()));
                modified = true;
            }
        }

        match mode {
            RoutingMode::Simple => {
                if let Some(rules) = routing.get_mut("rules").and_then(Value::as_array_mut) {
                    let before = rules.len();
                    rules.retain(|rule| rule.as_object().map(|object| !is_loopback_rule(object)).unwrap_or(true));
                    modified |= rules.len() != before;
                }
            }
            RoutingMode::Advanced => {
                if loopback_owner == Some(index) {
                    let rules = routing.entry("rules").or_insert_with(|| Value::Array(Vec::new()))
                        .as_array_mut().ok_or_else(|| format!("Поле routing.rules в {} должно быть массивом.", config_name))?;
                    modified |= ensure_loopback_rules(rules);
                }
            }
        }

        if modified {
            changed.insert(index);
        }
    }
    Ok(())
}

fn is_loopback_rule(rule: &serde_json::Map<String, Value>) -> bool {
    let Some(tags) = rule.get("inboundTag").and_then(Value::as_array) else { return false };
    tags.len() == 1 && matches!(
        tags[0].as_str(),
        Some(LOOPBACK_RULE_PRIMARY) | Some(LOOPBACK_RULE_RESERVE)
    )
}

fn ensure_loopback_rules(rules: &mut Vec<Value>) -> bool {
    let original = rules.clone();
    let mut primary = None;
    let mut reserve = None;
    let mut retained = Vec::with_capacity(rules.len() + 2);

    for rule in rules.drain(..) {
        let matched = rule.as_object().and_then(|object| {
            let tags = object.get("inboundTag")?.as_array()?;
            if tags.len() != 1 { return None; }
            match tags[0].as_str()? {
                LOOPBACK_RULE_PRIMARY => Some(true),
                LOOPBACK_RULE_RESERVE => Some(false),
                _ => None,
            }
        });

        match matched {
            Some(true) if primary.is_none() => {
                let mut rule = rule;
                let object = rule.as_object_mut().expect("matched rule is an object");
                object.insert("type".to_owned(), Value::String("field".to_owned()));
                object.insert("balancerTag".to_owned(), Value::String("xkeen-sub-balancer-rezerve".to_owned()));
                object.remove("outboundTag");
                primary = Some(rule);
            }
            Some(false) if reserve.is_none() => {
                let mut rule = rule;
                let object = rule.as_object_mut().expect("matched rule is an object");
                object.insert("type".to_owned(), Value::String("field".to_owned()));
                object.insert("balancerTag".to_owned(), Value::String("xkeen-sub-rest".to_owned()));
                object.remove("outboundTag");
                reserve = Some(rule);
            }
            Some(_) => {}
            None => retained.push(rule),
        }
    }

    let first = primary.unwrap_or_else(|| json!({
        "type": "field",
        "inboundTag": [LOOPBACK_RULE_PRIMARY],
        "balancerTag": "xkeen-sub-balancer-rezerve"
    }));
    let second = reserve.unwrap_or_else(|| json!({
        "type": "field",
        "inboundTag": [LOOPBACK_RULE_RESERVE],
        "balancerTag": "xkeen-sub-rest"
    }));

    retained.insert(0, second);
    retained.insert(0, first);
    let changed = original != retained;
    *rules = retained;
    changed
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

fn persist_changes(configs: &mut [ConfigFile], changed: &HashSet<usize>, no_persistent_backup: &HashSet<usize>) -> AppResult<()> {
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let mut backups = Vec::<(usize, PathBuf, bool)>::new();
    let mut written = Vec::<usize>::new();

    // Create all backups before writing any active configuration.
    for index in changed {
        let config = &configs[*index];
        let existed = config.path.exists();
        let backup = if no_persistent_backup.contains(index) {
            let temp_backup = PathBuf::from(format!("/tmp/{}.xkeen-sub-install-{nonce}.rollback", config.name));
            if existed {
                fs::copy(&config.path, &temp_backup)
                    .map_err(|error| format!("Не удалось подготовить откат {}: {error}", config.path.display()))?;
            }
            temp_backup
        } else {
            let persistent = PathBuf::from(format!("{}.xkeen-sub-install-{nonce}.bak", config.path.display()));
            if existed {
                fs::copy(&config.path, &persistent)
                    .map_err(|error| format!("Не удалось создать резервную копию {}: {error}", config.path.display()))?;
            }
            persistent
        };
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

    for (index, backup, existed) in backups {
        if no_persistent_backup.contains(&index) {
            if existed { let _ = fs::remove_file(&backup); }
        } else if existed {
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
    fn user_balancers_are_detected_even_if_program_tags_are_duplicated() {
        assert_eq!(inspect(json!([
            { "tag": "xkeen-sub-balancer" },
            { "tag": "xkeen-sub-balancer" },
            { "tag": "custom-balancer" }
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

    #[test]
    fn advanced_loopback_rules_are_inserted_in_order_before_user_rules() {
        let mut rules = vec![
            json!({"type":"field","domain":["example.com"],"outboundTag":"direct"})
        ];
        assert!(super::ensure_loopback_rules(&mut rules));
        assert_eq!(rules[0]["inboundTag"], json!(["loopback-in"]));
        assert_eq!(rules[0]["balancerTag"], "xkeen-sub-balancer-rezerve");
        assert_eq!(rules[1]["inboundTag"], json!(["loopback-in-rezerve"]));
        assert_eq!(rules[1]["balancerTag"], "xkeen-sub-rest");
        assert_eq!(rules[2]["domain"], json!(["example.com"]));
        assert!(!super::ensure_loopback_rules(&mut rules));
    }

    #[test]
    fn missing_second_loopback_rule_is_inserted_after_existing_first_rule() {
        let mut rules = vec![
            json!({"type":"field","inboundTag":["loopback-in"],"balancerTag":"wrong"}),
            json!({"domain":["example.com"],"outboundTag":"direct"})
        ];
        assert!(super::ensure_loopback_rules(&mut rules));
        assert_eq!(rules[0]["inboundTag"], json!(["loopback-in"]));
        assert_eq!(rules[1]["inboundTag"], json!(["loopback-in-rezerve"]));
        assert_eq!(rules[2]["domain"], json!(["example.com"]));
        assert!(!super::ensure_loopback_rules(&mut rules));
    }

    #[test]
    fn selected_template_redirects_proxy_routes_but_preserves_direct_and_block() {
        let mut configs = vec![super::ConfigFile {
            path: std::path::PathBuf::from("06_policy.json"),
            name: "06_policy.json".to_owned(),
            original: String::new(),
            document: json!({
                "outbounds": [
                    {"tag":"proxy-a","protocol":"vless"},
                    {"tag":"direct","protocol":"freedom"},
                    {"tag":"block","protocol":"blackhole"}
                ],
                "routing": {
                    "balancers": [],
                    "rules": [
                        {"type":"field","outboundTag":"proxy-a","domain":["example.com"]},
                        {"type":"field","outboundTag":"direct","network":"tcp"},
                        {"type":"field","outboundTag":"block","domain":["blocked.example"]},
                        {"type":"field","balancerTag":"old-user-balancer","ip":["1.2.3.4"]}
                    ]
                }
            })
        }];
        let mut changed = std::collections::HashSet::new();
        super::apply_routing_mode(
            &mut configs,
            super::RoutingMode::Simple,
            Some(0),
            &mut changed
        ).unwrap();

        let rules = configs[0].document["routing"]["rules"].as_array().unwrap();
        assert_eq!(rules[0]["balancerTag"], super::PROGRAM_BALANCER_TAG);
        assert_eq!(rules[0].get("outboundTag"), None);
        assert_eq!(rules[0]["domain"], json!(["example.com"]));
        assert_eq!(rules[1]["outboundTag"], "direct");
        assert_eq!(rules[2]["outboundTag"], "block");
        assert_eq!(rules[3]["balancerTag"], super::PROGRAM_BALANCER_TAG);
        assert_eq!(rules[3]["ip"], json!(["1.2.3.4"]));
    }

    #[test]
    fn advanced_loopback_rules_are_only_added_to_the_balancer_owner_file() {
        let mut configs = vec![
            super::ConfigFile {
                path: std::path::PathBuf::from("06_policy.json"),
                name: "06_policy.json".to_owned(),
                original: String::new(),
                document: json!({"routing":{"rules":[{"outboundTag":"proxy"}]}}),
            },
            super::ConfigFile {
                path: std::path::PathBuf::from("07_custom.json"),
                name: "07_custom.json".to_owned(),
                original: String::new(),
                document: json!({"routing":{"rules":[{"outboundTag":"proxy"}]}}),
            }
        ];
        let mut changed = std::collections::HashSet::new();
        super::apply_routing_mode(
            &mut configs,
            super::RoutingMode::Advanced,
            Some(0),
            &mut changed
        ).unwrap();

        assert_eq!(configs[0].document["routing"]["rules"][0]["inboundTag"], json!(["loopback-in"]));
        assert_eq!(configs[0].document["routing"]["rules"][1]["inboundTag"], json!(["loopback-in-rezerve"]));
        assert_eq!(configs[1].document["routing"]["rules"].as_array().unwrap().len(), 1);
        assert_eq!(configs[1].document["routing"]["rules"][0]["balancerTag"], super::PROGRAM_BALANCER_TAG);
    }

}
