use std::collections::HashMap;
use std::error::Error;

use serde_json::{json, Value};

use crate::backup::{BackupSession, BackupSource};
use crate::balancer::{BalancingMode, PROGRAM_BALANCER_TAG};
use crate::observatory::prepare_observatory_with_interval;
use crate::terminal::{info, success, warning, ProgressBar};
use crate::update_apply;
use crate::update_config::{load_config_documents, ConfigDocument, CONFIG_DIR, OUTBOUND_FILE};
use crate::update_merge::{collect_selector_tags, ensure_service_outbounds, ArchivePlan};
use crate::update_plan::build_file_plan;
use crate::update_state::RunState;
use crate::xray_test::{test_config_set_detailed, XrayConfigInput};

type AppResult<T> = Result<T, Box<dyn Error>>;

const RESERVE_TAG: &str = "xkeen-sub-balancer-rezerve";
const REST_TAG: &str = "xkeen-sub-rest";
const LOOPBACK_PRIMARY: &str = "loopback-in";
const LOOPBACK_RESERVE: &str = "loopback-in-rezerve";

pub(crate) fn current_mode_label() -> String {
    match load_config_documents() {
        Ok(docs) => match detect_documents_mode(&docs) {
            Ok(mode) => mode_label(mode).to_owned(),
            Err(error) => format!("Не удалось определить ({error})"),
        },
        Err(error) => format!("Не удалось определить ({error})"),
    }
}

fn detect_documents_mode(docs: &[ConfigDocument]) -> AppResult<BalancingMode> {
    let routing_docs: Vec<&ConfigDocument> = docs.iter()
        .filter(|doc| doc.value.get("routing").is_some())
        .collect();
    if routing_docs.is_empty() { return Err("Не найдены конфигурации routing.".into()); }
    let configured: Vec<&ConfigDocument> = routing_docs.into_iter()
        .filter(|doc| doc.value.get("routing").and_then(|r| r.get("balancers"))
            .and_then(Value::as_array).map(|items| !items.is_empty()).unwrap_or(false))
        .collect();
    if configured.is_empty() { return Ok(BalancingMode::NotConfigured); }
    let mut modes = configured.into_iter().map(|doc| crate::balancer::detect_balancing_mode(&doc.value));
    let mut result = modes.next().unwrap_or(BalancingMode::NotConfigured);
    for mode in modes {
        if mode == BalancingMode::Custom { return Ok(BalancingMode::Custom); }
        if mode != result {
            return Ok(BalancingMode::Ambiguous);
        }
    }
    Ok(result)
}

fn mode_label(mode: BalancingMode) -> &'static str {
    match mode {
        BalancingMode::NotConfigured => "Не настроен",
        BalancingMode::Simple => "Простой",
        BalancingMode::Advanced => "Продвинутый",
        BalancingMode::Custom => "Пользовательский",
        BalancingMode::IncompleteAdvanced => "Неполный продвинутый",
        BalancingMode::Ambiguous => "Неоднозначная конфигурация",
    }
}

pub(crate) fn switch_mode(target: BalancingMode) -> AppResult<()> {
    if target == BalancingMode::Custom {
        info("Пользовательский режим определяется автоматически. Выбрать его вручную нельзя.");
        return Ok(());
    }

    let _lock = crate::update_lock::FileLock::acquire("/opt/var/run/xkeen-sub-update.lock")?
        .ok_or("Обновление или другая операция с конфигурацией уже выполняется. Повторите позже.")?;

    let mut docs = load_config_documents()?;
    let current = detect_documents_mode(&docs)?;
    if current == target {
        success(&format!("Режим «{}» уже используется. Переключение не требуется.", mode_label(target)));
        return Ok(());
    }
    if matches!(current, BalancingMode::IncompleteAdvanced | BalancingMode::Ambiguous) {
        return Err(format!("Нельзя безопасно переключить режим из состояния «{}». Сначала исправьте структуру балансировщиков.", mode_label(current)).into());
    }

    warning(&format!("Текущий режим: {}. Будет установлен режим: {}.", mode_label(current), mode_label(target)));
    if current == BalancingMode::Custom {
        warning("Пользовательские балансировщики будут полностью заменены шаблоном xkeen-sub. Пользовательские правила маршрутизации будут преобразованы по логике установщика.");
    }
    loop {
        match crate::terminal::read_input("Продолжить? (1 — да, 2 — нет): ")?.as_str() {
            "1" => break,
            "2" | "0" => { info("Переключение отменено. Конфигурация не изменена."); return Ok(()); }
            _ => warning("Введите 1 или 2."),
        }
    }

    let mut backup = BackupSession::start("Переключение режима балансировки")?;
    backup.event("BALANCER", &format!("Переключение режима: {} -> {}.", mode_label(current), mode_label(target)))?;

    let result = prepare_and_apply(&mut docs, target, &mut backup);
    if let Err(ref failure) = result {
        if backup.is_running() {
            backup.set_outcome("ОШИБКА", &format!("Переключение не завершено: {failure}"), true);
        }
    }
    if let Err(error) = crate::current_connection::reset_after_update() {
        warning(&format!("Не удалось сбросить ручной выбор подключения: {error}"));
    }
    let backup_id = backup.id().to_owned();
    let finish_result = backup.finish();
    info(&format!("Резервная запись: {backup_id}."));
    finish_result?;
    result
}

fn prepare_and_apply(docs: &mut Vec<ConfigDocument>, target: BalancingMode, backup: &mut BackupSession) -> AppResult<()> {
    let outbound_index = docs.iter().position(|doc| doc.file_name == OUTBOUND_FILE)
        .ok_or_else(|| format!("Не найден файл {OUTBOUND_FILE}."))?;
    let mut outbounds = docs[outbound_index].value.get("outbounds")
        .and_then(Value::as_array).cloned()
        .ok_or("В рабочем Outbound отсутствует массив outbounds.")?;
    ensure_service_outbounds(&mut outbounds)?;
    let outbound_root = docs[outbound_index].value.as_object_mut()
        .ok_or("Корень Outbound должен быть JSON-объектом.")?;
    outbound_root.insert("outbounds".to_owned(), Value::Array(outbounds.clone()));

    let api_tags = crate::update_config::collect_api_tags(docs);
    let tags = collect_selector_tags(&outbounds, &api_tags)?;
    let freedom_tags: Vec<String> = outbounds.iter()
        .filter(|item| item.get("protocol").and_then(Value::as_str) == Some("freedom"))
        .filter_map(|item| item.get("tag").and_then(Value::as_str)).map(str::to_owned).collect();
    let selector_tags = if tags.is_empty() { freedom_tags.clone() } else { tags.clone() };
    let protocols: HashMap<String, String> = outbounds.iter().filter_map(|item| Some((
        item.get("tag")?.as_str()?.to_owned(),
        item.get("protocol")?.as_str()?.to_owned()
    ))).collect();

    let routing_indices: Vec<usize> = docs.iter().enumerate()
        .filter_map(|(index, doc)| doc.value.get("routing").is_some().then_some(index)).collect();
    if routing_indices.is_empty() { return Err("Не найдена конфигурация routing.".into()); }
    let owner = routing_indices.iter().copied().find(|index| {
        docs[*index].value["routing"].get("balancers").and_then(Value::as_array).map(|v| !v.is_empty()).unwrap_or(false)
    }).unwrap_or(routing_indices[0]);

    for index in &routing_indices {
        let config_name = docs[*index].file_name.clone();
        let routing = docs[*index].value.get_mut("routing").and_then(Value::as_object_mut)
            .ok_or_else(|| format!("Поле routing в {config_name} должно быть объектом."))?;
        let rules = routing.get_mut("rules");
        if let Some(value) = rules {
            let rules = value.as_array_mut().ok_or("Поле routing.rules должно быть массивом.")?;
            let mut transformed = 0usize;
            for rule in rules.iter_mut() {
                let Some(object) = rule.as_object_mut() else { continue };
                if is_loopback_rule(object) { continue; }
                if object.contains_key("balancerTag") {
                    object.insert("balancerTag".to_owned(), Value::String(PROGRAM_BALANCER_TAG.to_owned()));
                    object.remove("outboundTag");
                    transformed += 1;
                    continue;
                }
                let Some(tag) = object.get("outboundTag").and_then(Value::as_str).map(str::to_owned) else { continue };
                if api_tags.contains(&tag) || ["api", "direct", "block"].contains(&tag.as_str()) { continue; }
                if protocols.get(&tag).map(|p| ["freedom", "blackhole", "dns", "loopback"].contains(&p.as_str())).unwrap_or(false) { continue; }
                object.remove("outboundTag");
                object.insert("balancerTag".to_owned(), Value::String(PROGRAM_BALANCER_TAG.to_owned()));
                transformed += 1;
            }
            backup.event("BALANCER", &format!("{config_name}: преобразовано правил маршрутизации: {transformed}."))?;
        }
    }

    let main_balancer = json!({
        "tag": PROGRAM_BALANCER_TAG,
        "selector": selector_tags.clone(),
        "fallbackTag": if target == BalancingMode::Advanced { "loopback-out" } else { "direct" },
        "strategy": { "type": "leastPing" }
    });
    let balancers = if target == BalancingMode::Simple {
        json!([main_balancer])
    } else {
        json!([
            main_balancer,
            {"tag":RESERVE_TAG,"selector":selector_tags.clone(),"fallbackTag":"loopback-out-rezerve","strategy":{"type":"random"}},
            {"tag":REST_TAG,"selector":selector_tags.clone(),"fallbackTag":"direct","strategy":{"type":"random"}}
        ])
    };
    for index in &routing_indices {
        let routing = docs[*index].value["routing"].as_object_mut().ok_or("Поле routing должно быть объектом.")?;
        if *index == owner {
            routing.insert("balancers".to_owned(), balancers.clone());
        } else if routing.contains_key("balancers") {
            routing.insert("balancers".to_owned(), Value::Array(Vec::new()));
        }
        if let Some(rules) = routing.get_mut("rules").and_then(Value::as_array_mut) {
            if target == BalancingMode::Simple {
                rules.retain(|rule| rule.as_object().map(|obj| !is_loopback_rule(obj)).unwrap_or(true));
            }
        }
    }

    if target == BalancingMode::Advanced {
        let rules = docs[owner].value["routing"].as_object_mut().unwrap()
            .entry("rules").or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut().ok_or("Поле routing.rules должно быть массивом.")?;
        ensure_loopback_rules(rules);
    }

    let initial_obs_tags = selector_tags.clone();
    let mut obs_docs: Vec<(String, Value)> = docs.iter().map(|doc| (doc.file_name.clone(), doc.value.clone())).collect();
    prepare_observatory_with_interval(&mut obs_docs, &initial_obs_tags, if target == BalancingMode::Advanced { Some("10s") } else { None })?;
    for (name, value) in obs_docs {
        if let Some(index) = docs.iter().position(|doc| doc.file_name == name) {
            docs[index].value = value;
        } else {
            docs.push(ConfigDocument {
                file_name: name.clone(),
                path: std::path::Path::new(CONFIG_DIR).join(&name),
                original: String::new(),
                original_value: Value::Null,
                value,
                is_new: true,
            });
        }
    }

    validate_documents(docs)?;
    if target == BalancingMode::Advanced && !tags.is_empty() {
        info("Проверяем подключения для Advanced блоками по пять...");
        let probe_docs: Vec<(String, Value)> = docs.iter().map(|doc| (doc.file_name.clone(), doc.value.clone())).collect();
        let selected = crate::xray_probe::select_advanced_outbounds(&probe_docs, &tags)?;
        if !selected.is_empty() {
            let main = vec![selected[0].0.clone()];
            let reserve = selected.get(1).map(|item| vec![item.0.clone()]).unwrap_or_else(|| freedom_tags.clone());
            let mut excluded = vec![selected[0].0.as_str()];
            if let Some(second) = selected.get(1) { excluded.push(second.0.as_str()); }
            let rest = if tags.len() <= 2 { freedom_tags.clone() } else {
                tags.iter().filter(|tag| !excluded.contains(&tag.as_str())).cloned().collect()
            };
            let mut obs_docs: Vec<(String, Value)> = docs.iter().map(|doc| (doc.file_name.clone(), doc.value.clone())).collect();
            prepare_observatory_with_interval(&mut obs_docs, &main, Some("10s"))?;
            for (name, value) in obs_docs {
                if let Some(index) = docs.iter().position(|doc| doc.file_name == name) { docs[index].value = value; }
            }
            backup.event("BALANCER", &format!("Advanced selectors: main={}, reserve={}, rest={}.", main[0], reserve.first().map(String::as_str).unwrap_or("freedom"), rest.len()))?;
            apply_advanced_selectors(&mut docs[owner].value, &main, &reserve, &rest)?;
        } else {
            warning("Не удалось получить успешные результаты Observatory. В селекторах оставлены все доступные подключения.");
        }
    }

    validate_documents(docs)?;
    let state = RunState::default();
    let archive = ArchivePlan { outbounds: Vec::new(), added: 0 };
    let plan = build_file_plan(docs, &state, &archive)?;
    backup.set_plan_count(plan.len());
    backup.event("PLAN", &format!("Подготовлен план переключения: {} файлов.", plan.len()))?;
    let sources: Vec<BackupSource> = plan.iter()
        .filter(|op| op.kind != update_apply::PlannedOperationKind::Create)
        .map(|op| BackupSource { path: op.path.clone(), category: op.category.to_owned() }).collect();
    let total = sources.len() + plan.len() * 3 + 2;
    let mut progress = ProgressBar::new(total);
    backup.backup_files_with_progress(&sources, &mut progress)?;
    update_apply::execute(&plan, backup, &mut progress)?;
    progress.finish("Режим балансировки переключён");
    backup.set_outcome("УСПЕШНО", "Новый режим проверен Xray, применён; XKeen перезапущен и проверен.", true);
    success("Режим балансировки успешно переключён.");
    Ok(())
}

fn validate_documents(docs: &[ConfigDocument]) -> AppResult<()> {
    let inputs: Vec<XrayConfigInput> = docs.iter().map(|doc| {
        let content = serde_json::to_string_pretty(&doc.value).unwrap_or_else(|_| doc.original.clone());
        XrayConfigInput::new(doc.file_name.clone(), content)
    }).collect();
    test_config_set_detailed(&inputs).map(|_| ()).map_err(|err| {
        format!("Итоговая конфигурация не прошла проверку Xray. Рабочие файлы не изменены: {err}").into()
    })
}

fn apply_advanced_selectors(document: &mut Value, main: &[String], reserve: &[String], rest: &[String]) -> AppResult<()> {
    let balancers = document.get_mut("routing").and_then(Value::as_object_mut)
        .and_then(|routing| routing.get_mut("balancers")).and_then(Value::as_array_mut)
        .ok_or("Не найден массив Advanced-балансировщиков.")?;
    for balancer in balancers {
        let tag = balancer.get("tag").and_then(Value::as_str).unwrap_or("");
        let selector = match tag {
            PROGRAM_BALANCER_TAG => Some(main),
            RESERVE_TAG => Some(reserve),
            REST_TAG => Some(rest),
            _ => None,
        };
        if let Some(selector) = selector {
            balancer.as_object_mut().ok_or("Балансировщик должен быть объектом.")?
                .insert("selector".to_owned(), Value::Array(selector.iter().cloned().map(Value::String).collect()));
        }
    }
    Ok(())
}

fn is_loopback_rule(rule: &serde_json::Map<String, Value>) -> bool {
    let Some(tags) = rule.get("inboundTag").and_then(Value::as_array) else { return false };
    tags.len() == 1 && matches!(tags[0].as_str(), Some(LOOPBACK_PRIMARY) | Some(LOOPBACK_RESERVE))
}

fn ensure_loopback_rules(rules: &mut Vec<Value>) {
    let primary = json!({"type":"field","inboundTag":[LOOPBACK_PRIMARY],"balancerTag":RESERVE_TAG});
    let reserve = json!({"type":"field","inboundTag":[LOOPBACK_RESERVE],"balancerTag":REST_TAG});
    let mut user_rules = Vec::new();
    for rule in rules.drain(..) {
        if !rule.as_object().map(is_loopback_rule).unwrap_or(false) { user_rules.push(rule); }
    }
    rules.push(primary);
    rules.push(reserve);
    rules.extend(user_rules);
}
