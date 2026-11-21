use chrono::{Local, TimeZone};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;
use std::error::Error;
use std::fs;
use std::path::Path;
use std::process::Command;

use crate::terminal::{error, info, menu_item, read_input, success, title, warning};

type AppResult<T> = Result<T, Box<dyn Error>>;

const CONFIG_DIR: &str = "/opt/etc/xray/configs";
const XRAY_BIN: &str = "/opt/sbin/xray";
const SETTINGS_PATH: &str = "/opt/etc/xkeen-sub/node.json";
const STATE_PATH: &str = "/opt/etc/xkeen-sub/node-state.json";
const UPDATE_LOCK: &str = "/opt/var/run/xkeen-sub-update.lock";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct NodeSettings {
    #[serde(default = "default_return_after_seconds")]
    return_after_seconds: Option<u64>,
}

fn default_return_after_seconds() -> Option<u64> {
    Some(600)
}

impl Default for NodeSettings {
    fn default() -> Self {
        Self { return_after_seconds: default_return_after_seconds() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProcessIdentity {
    pid: u32,
    start_ticks: u64,
    boot_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ForcedSelection {
    balancer_tag: String,
    outbound_tag: String,
    selected_at: i64,
    expires_at: Option<i64>,
    process: ProcessIdentity,
}

struct CurrentContext {
    server: String,
    balancer: String,
    selected_tag: String,
}

pub fn run() -> AppResult<()> {
    tick()?;
    loop {
        println!();
        title("XKEEN-SUB — ТЕКУЩИЙ УЗЕЛ");
        let context = show_current()?;
        show_forced_status()?;
        println!();
        menu_item("1", "Повторная проверка");
        menu_item("2", "Сменить подключение");
        menu_item("3", "Настройка");
        menu_item("0", "Назад");

        match read_input("Выберите пункт: ")?.as_str() {
            "1" => {
                let refreshed = show_current()?;
                info(&format!("Текущий тэг: {}", refreshed.selected_tag));
            }
            "2" => change_connection(&context)?,
            "3" => settings_menu()?,
            "0" => return Ok(()),
            _ => error("Введите цифру от 0 до 3."),
        }
    }
}

fn show_current() -> AppResult<CurrentContext> {
    let configs = load_configs()?;
    let server = find_api_server(&configs)?;
    let balancer = find_active_balancer(&configs)?;
    let selected_tag = query_balancer(&server, &balancer)?;
    info(&format!("Тэг: {selected_tag}"));
    Ok(CurrentContext { server, balancer, selected_tag })
}

fn show_forced_status() -> AppResult<()> {
    let Some(state) = load_forced_selection()? else {
        info("Режим: автоматический выбор.");
        return Ok(());
    };

    match state.expires_at {
        Some(expires_at) => {
            let time = Local.timestamp_opt(expires_at, 0).single()
                .map(|value| value.format("%d.%m.%Y %H:%M").to_string())
                .unwrap_or_else(|| expires_at.to_string());
            info(&format!("Режим: принудительный до {time}."));
        }
        None => info("Режим: принудительный до перезагрузки Xray или обновления подписок."),
    }
    Ok(())
}

fn change_connection(context: &CurrentContext) -> AppResult<()> {
    let store = crate::subscriptions::load_subscriptions()?;
    if store.subscriptions.is_empty() {
        warning("Подписок пока нет. Сначала добавьте подписку.");
        return Ok(());
    }

    let configs = load_configs()?;
    let blacklist = crate::blacklist::load()?;
    let mut choices = Vec::new();

    title("ВЫБОР ПОДПИСКИ");
    for (index, subscription) in store.subscriptions.iter().enumerate() {
        let tags = available_tags(&configs, subscription, &blacklist);
        let label = subscription.name.as_deref()
            .filter(|name| !name.trim().is_empty())
            .unwrap_or(&subscription.prefix);
        menu_item(&(index + 1).to_string(), &format!("{label} — доступно подключений: {}", tags.len()));
        choices.push(tags);
    }
    menu_item("0", "Назад");

    let choice = read_input("Выберите подписку: ")?;
    if choice == "0" {
        return Ok(());
    }
    let index = match choice.parse::<usize>() {
        Ok(number) if number > 0 && number <= store.subscriptions.len() => number - 1,
        _ => {
            error("Подписки с таким номером нет.");
            return Ok(());
        }
    };

    let subscription = &store.subscriptions[index];
    let tags = &choices[index];
    if tags.is_empty() {
        warning("В этой подписке нет доступных подключений. Возможно, все они заблокированы или ещё не загружены.");
        return Ok(());
    }

    title(&format!("ПОДКЛЮЧЕНИЯ — {}", subscription.prefix));
    for (position, tag) in tags.iter().enumerate() {
        menu_item(&(position + 1).to_string(), tag);
    }
    menu_item("0", "Назад");

    let choice = read_input("Выберите подключение: ")?;
    if choice == "0" {
        return Ok(());
    }
    let node_index = match choice.parse::<usize>() {
        Ok(number) if number > 0 && number <= tags.len() => number - 1,
        _ => {
            error("Подключения с таким номером нет.");
            return Ok(());
        }
    };

    let target = &tags[node_index];
    force_target(&context.server, &context.balancer, target)?;
    success(&format!("Подключение {} выбрано принудительно.", target));
    show_forced_status()?;
    Ok(())
}

fn available_tags(
    configs: &[Value],
    subscription: &crate::subscriptions::Subscription,
    blacklist: &crate::blacklist::BlacklistStore,
) -> Vec<String> {
    let mut tags = BTreeSet::new();
    for config in configs {
        let Some(outbounds) = config.get("outbounds").and_then(Value::as_array) else {
            continue;
        };
        for outbound in outbounds {
            let Some(tag) = outbound.get("tag").and_then(Value::as_str) else {
                continue;
            };
            let Some(original_tag) = tag.strip_prefix(&subscription.prefix) else {
                continue;
            };
            if !original_tag.is_empty() && !blacklist.is_blocked(&subscription.id, original_tag) {
                tags.insert(tag.to_owned());
            }
        }
    }
    tags.into_iter().collect()
}

fn settings_menu() -> AppResult<()> {
    loop {
        let mut settings = load_settings()?;
        title("НАСТРОЙКА ПРИНУДИТЕЛЬНОГО ПОДКЛЮЧЕНИЯ");
        info(&format!("Текущая настройка: {}", describe_duration(settings.return_after_seconds)));
        println!();
        menu_item("1", "10 минут (по умолчанию)");
        menu_item("2", "30 минут");
        menu_item("3", "1 час");
        menu_item("4", "3 часа");
        menu_item("5", "6 часов");
        menu_item("6", "12 часов");
        menu_item("7", "24 часа");
        menu_item("8", "До ближайшей перезагрузки Xray или обновления подписок");
        menu_item("0", "Назад");

        let value = read_input("Выберите время: ")?;
        let selected = match value.as_str() {
            "1" => Some(600),
            "2" => Some(1800),
            "3" => Some(3600),
            "4" => Some(10800),
            "5" => Some(21600),
            "6" => Some(43200),
            "7" => Some(86400),
            "8" => None,
            "0" => return Ok(()),
            _ => {
                error("Введите цифру от 0 до 8.");
                continue;
            }
        };
        settings.return_after_seconds = selected;
        save_settings(&settings)?;
        success(&format!(
            "Настройка сохранена. Она будет применена при следующем ручном переключении: {}.",
            describe_duration(settings.return_after_seconds)
        ));
    }
}

fn describe_duration(seconds: Option<u64>) -> String {
    match seconds {
        Some(600) => "10 минут".to_owned(),
        Some(1800) => "30 минут".to_owned(),
        Some(3600) => "1 час".to_owned(),
        Some(10800) => "3 часа".to_owned(),
        Some(21600) => "6 часов".to_owned(),
        Some(43200) => "12 часов".to_owned(),
        Some(86400) => "24 часа".to_owned(),
        None => "до перезагрузки Xray или обновления подписок".to_owned(),
        Some(value) => format!("{value} секунд"),
    }
}

fn load_settings() -> AppResult<NodeSettings> {
    if !Path::new(SETTINGS_PATH).exists() {
        return Ok(NodeSettings::default());
    }
    Ok(serde_json::from_slice(&fs::read(SETTINGS_PATH)?)?)
}

fn save_settings(settings: &NodeSettings) -> AppResult<()> {
    fs::create_dir_all("/opt/etc/xkeen-sub")?;
    let temporary = format!("{SETTINGS_PATH}.tmp.{}", std::process::id());
    let mut file = fs::File::create(&temporary)?;
    use std::io::Write;
    file.write_all(&serde_json::to_vec_pretty(settings)?)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    fs::rename(temporary, SETTINGS_PATH)?;
    Ok(())
}

fn force_target(server: &str, balancer: &str, target: &str) -> AppResult<()> {
    let process = current_xray_process()
        .ok_or("Не удалось определить работающий процесс Xray. Принудительное подключение не применено.")?;
    let settings = load_settings()?;
    let now = Local::now().timestamp();
    let expires_at = settings.return_after_seconds
        .map(|seconds| now.saturating_add(seconds.min(i64::MAX as u64) as i64));

    set_balancer_target(server, balancer, Some(target))?;

    let state = ForcedSelection {
        balancer_tag: balancer.to_owned(),
        outbound_tag: target.to_owned(),
        selected_at: now,
        expires_at,
        process,
    };
    if let Err(error) = save_forced_selection(&state) {
        let _ = set_balancer_target(server, balancer, None);
        return Err(error);
    }
    if let Err(error) = crate::auto_update::refresh_cron() {
        let _ = set_balancer_target(server, balancer, None);
        let _ = fs::remove_file(STATE_PATH);
        return Err(error);
    }
    Ok(())
}

fn set_balancer_target(server: &str, balancer: &str, target: Option<&str>) -> AppResult<()> {
    let mut command = Command::new(XRAY_BIN);
    command.args(["api", "bo"])
        .arg(format!("--server={server}"))
        .args(["--timeout=3", "-b", balancer]);
    if let Some(target) = target {
        command.arg(target);
    } else {
        command.arg("-r");
    }
    let output = command.output()
        .map_err(|error| format!("Не удалось запустить команду управления балансировщиком Xray: {error}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let details = if !stderr.trim().is_empty() { stderr.trim() } else { stdout.trim() };
        return Err(format!(
            "Xray не смог изменить выбор балансировщика {balancer}: {}",
            if details.is_empty() { output.status.to_string() } else { details.to_owned() }
        ).into());
    }
    Ok(())
}

pub(crate) fn tick() -> AppResult<()> {
    let Some(state) = load_forced_selection()? else {
        return Ok(());
    };
    let _update = match crate::update_lock::FileLock::acquire(UPDATE_LOCK)? {
        Some(lock) => lock,
        None => return Ok(()),
    };

    let Some(process) = current_xray_process() else {
        // Xray has stopped; a subsequent process starts without the in-memory override.
        remove_forced_selection()?;
        return Ok(());
    };
    if !same_process(&state.process, &process) {
        // Xray restarted. RoutingService clears the override on process restart.
        remove_forced_selection()?;
        return Ok(());
    }

    if state.expires_at.map(|expires| expires <= Local::now().timestamp()).unwrap_or(false) {
        let configs = load_configs()?;
        let server = find_api_server(&configs)?;
        set_balancer_target(&server, &state.balancer_tag, None)?;
        remove_forced_selection()?;
    }
    Ok(())
}

pub(crate) fn reset_after_update() -> AppResult<()> {
    let Some(state) = load_forced_selection()? else {
        return Ok(());
    };
    if let Some(process) = current_xray_process() {
        if same_process(&state.process, &process) {
            let configs = load_configs()?;
            let server = find_api_server(&configs)?;
            set_balancer_target(&server, &state.balancer_tag, None)?;
        }
    }
    remove_forced_selection()
}

pub(crate) fn has_pending_selection() -> bool {
    Path::new(STATE_PATH).is_file()
}

fn remove_forced_selection() -> AppResult<()> {
    match fs::remove_file(STATE_PATH) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    crate::auto_update::refresh_cron()
}

fn load_forced_selection() -> AppResult<Option<ForcedSelection>> {
    if !Path::new(STATE_PATH).exists() {
        return Ok(None);
    }
    let state = serde_json::from_slice(&fs::read(STATE_PATH)?)
        .map_err(|error| format!("Файл состояния текущего узла повреждён: {error}"))?;
    Ok(Some(state))
}

fn save_forced_selection(state: &ForcedSelection) -> AppResult<()> {
    fs::create_dir_all("/opt/etc/xkeen-sub")?;
    let temporary = format!("{STATE_PATH}.tmp.{}", std::process::id());
    let mut file = fs::File::create(&temporary)?;
    use std::io::Write;
    file.write_all(&serde_json::to_vec_pretty(state)?)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    fs::rename(temporary, STATE_PATH)?;
    Ok(())
}

fn current_xray_process() -> Option<ProcessIdentity> {
    let mut entries = fs::read_dir("/proc").ok()?.filter_map(Result::ok).collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let pid = entry.file_name().to_string_lossy().parse::<u32>().ok()?;
        let process_dir = entry.path();
        if fs::read_to_string(process_dir.join("comm")).ok()?.trim() != "xray" {
            continue;
        }
        let cmdline = fs::read(process_dir.join("cmdline")).ok()?;
        if !is_xray_proxy_cmdline(&cmdline) {
            continue;
        }
        let stat = fs::read_to_string(process_dir.join("stat")).ok()?;
        let (_, fields) = stat.rsplit_once(')')?;
        let start_ticks = fields.split_whitespace().nth(19)?.parse::<u64>().ok()?;
        let boot_id = fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .unwrap_or_default().trim().to_owned();
        return Some(ProcessIdentity { pid, start_ticks, boot_id });
    }
    None
}

fn is_xray_proxy_cmdline(cmdline: &[u8]) -> bool {
    let args = cmdline.split(|byte| *byte == 0)
        .filter(|arg| !arg.is_empty())
        .map(|arg| String::from_utf8_lossy(arg))
        .collect::<Vec<_>>();
    if args.is_empty() {
        return false;
    }
    !args.iter().skip(1).any(|arg| {
        matches!(arg.as_ref(), "api" | "version" | "help" | "convert" | "-test" | "--test" | "-dump" | "--dump")
            || arg.starts_with("-test=") || arg.starts_with("--test=")
            || arg.starts_with("-dump=") || arg.starts_with("--dump=")
    })
}

fn same_process(previous: &ProcessIdentity, current: &ProcessIdentity) -> bool {
    previous.pid == current.pid
        && previous.start_ticks == current.start_ticks
        && previous.boot_id == current.boot_id
}

fn load_configs() -> AppResult<Vec<Value>> {
    let dir = Path::new(CONFIG_DIR);
    if !dir.is_dir() {
        return Err(format!("Каталог конфигурации Xray не найден: {}", dir.display()).into());
    }

    let mut paths = fs::read_dir(dir)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && path.extension().and_then(|ext| ext.to_str())
                    .map(|ext| ext.eq_ignore_ascii_case("json") || ext.eq_ignore_ascii_case("jsonc"))
                    .unwrap_or(false)
        })
        .collect::<Vec<_>>();
    paths.sort();

    if paths.is_empty() {
        return Err("В каталоге Xray не найдены файлы JSON/JSONC.".into());
    }

    paths.into_iter().map(|path| {
        let content = fs::read_to_string(&path)
            .map_err(|error| format!("Не удалось прочитать {}: {error}", path.display()))?;
        json5::from_str(&content)
            .map_err(|error| format!("Не удалось разобрать {}: {error}", path.display()).into())
    }).collect()
}

fn find_api_server(configs: &[Value]) -> AppResult<String> {
    let api = configs.iter().find_map(|config| config.get("api"))
        .ok_or("В конфигурации Xray не включён API. Для управления текущим подключением требуется API с RoutingService.")?;

    let services = api.get("services").and_then(Value::as_array)
        .ok_or("В настройках Xray API не найден список services.")?;
    if !services.iter().any(|service| service.as_str() == Some("RoutingService")) {
        return Err("В Xray API не включён RoutingService. Добавьте его в api.services.".into());
    }

    if let Some(listen) = api.get("listen").and_then(Value::as_str) {
        return validate_server_address(listen);
    }

    let api_tag = api.get("tag").and_then(Value::as_str)
        .ok_or("В настройках Xray API отсутствует tag, необходимый для поиска API inbound.")?;

    for config in configs {
        let Some(inbounds) = config.get("inbounds").and_then(Value::as_array) else {
            continue;
        };
        for inbound in inbounds {
            if inbound.get("tag").and_then(Value::as_str) != Some(api_tag) {
                continue;
            }
            let port = inbound.get("port").and_then(Value::as_u64)
                .ok_or("У API inbound не задан корректный port.")?;
            let host = inbound.get("listen").and_then(Value::as_str).unwrap_or("127.0.0.1");
            return validate_server_address(&format!("{host}:{port}"));
        }
    }

    Err("Не удалось определить адрес Xray API: нет api.listen и не найден inbound с tag API.".into())
}

fn validate_server_address(address: &str) -> AppResult<String> {
    let address = address.trim();
    if address.is_empty() || !address.rsplit_once(':').map(|(_, port)| port.parse::<u16>().is_ok()).unwrap_or(false) {
        return Err(format!("Некорректный адрес Xray API: {address}").into());
    }
    Ok(address.to_owned())
}

fn find_active_balancer(configs: &[Value]) -> AppResult<String> {
    let mut configured = BTreeSet::new();
    let mut referenced = BTreeSet::new();

    for config in configs {
        if let Some(balancers) = config.get("routing").and_then(|routing| routing.get("balancers")).and_then(Value::as_array) {
            for balancer in balancers {
                if let Some(tag) = balancer.get("tag").and_then(Value::as_str) {
                    configured.insert(tag.to_owned());
                }
            }
        }
        if let Some(rules) = config.get("routing").and_then(|routing| routing.get("rules")).and_then(Value::as_array) {
            for rule in rules {
                if let Some(tag) = rule.get("balancerTag").and_then(Value::as_str) {
                    referenced.insert(tag.to_owned());
                }
            }
        }
    }

    let active = referenced.intersection(&configured).cloned().collect::<Vec<_>>();
    if active.len() == 1 {
        return Ok(active[0].clone());
    }
    if active.len() > 1 {
        return Err(format!(
            "В routing.rules используются несколько балансировщиков ({}). Нельзя определить один текущий тэг без контекста конкретного запроса.",
            active.join(", ")
        ).into());
    }

    match configured.len() {
        0 => Err("В конфигурации Xray не найден ни один балансировщик.".into()),
        1 => Ok(configured.iter().next().expect("one configured balancer").clone()),
        _ => Err("Не удалось однозначно определить балансировщик: ни один из настроенных балансировщиков не указан в routing.rules.".into()),
    }
}

fn query_balancer(server: &str, balancer: &str) -> AppResult<String> {
    if !Path::new(XRAY_BIN).is_file() {
        return Err(format!("Исполняемый файл Xray не найден: {XRAY_BIN}").into());
    }

    let server_arg = format!("--server={server}");
    let output = Command::new(XRAY_BIN)
        .args(["api", "bi"])
        .arg(server_arg)
        .args(["--timeout=3", "--json", balancer])
        .output()
        .map_err(|error| format!("Не удалось запустить Xray API: {error}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let details = if !stderr.trim().is_empty() { stderr.trim() } else { stdout.trim() };
        let reason = if details.is_empty() {
            format!("код завершения {}", output.status)
        } else {
            details.to_owned()
        };
        return Err(format!("Не удалось получить состояние балансировщика {balancer} через Xray API ({reason}). Проверьте, запущен ли Xray и доступен ли API.").into());
    }

    let response: Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("Xray API вернул неожиданный ответ: {error}"))?;
    parse_selected_tag(&response)
        .ok_or_else(|| format!("Xray API не сообщил выбранный outbound для балансировщика {balancer}.").into())
}

fn parse_selected_tag(response: &Value) -> Option<String> {
    let balancer = response.get("balancer").unwrap_or(response);
    if let Some(target) = balancer.get("override")
        .and_then(|value| value.get("target"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    {
        return Some(target.to_owned());
    }

    balancer.get("principleTarget")
        .and_then(|value| value.get("tag"))
        .and_then(Value::as_array)
        .and_then(|tags| tags.first())
        .and_then(Value::as_str)
        .filter(|tag| !tag.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::{default_return_after_seconds, describe_duration, is_xray_proxy_cmdline, parse_selected_tag, same_process, ProcessIdentity};
    use serde_json::json;

    #[test]
    fn selected_tag_uses_strategy_first_target() {
        let response = json!({
            "balancer": {
                "principleTarget": { "tag": ["sub1--node-a", "sub1--node-b"] }
            }
        });
        assert_eq!(parse_selected_tag(&response).as_deref(), Some("sub1--node-a"));
    }

    #[test]
    fn manual_override_has_priority_over_strategy() {
        let response = json!({
            "balancer": {
                "override": { "target": "sub2--forced" },
                "principleTarget": { "tag": ["sub1--node-a"] }
            }
        });
        assert_eq!(parse_selected_tag(&response).as_deref(), Some("sub2--forced"));
    }

    #[test]
    fn empty_balancer_response_has_no_selected_tag() {
        assert_eq!(parse_selected_tag(&json!({"balancer": {}})), None);
    }

    #[test]
    fn default_manual_selection_duration_is_ten_minutes() {
        assert_eq!(default_return_after_seconds(), Some(600));
        assert_eq!(describe_duration(None), "до перезагрузки Xray или обновления подписок");
    }

    #[test]
    fn process_identity_detects_xray_restart() {
        let previous = ProcessIdentity { pid: 10, start_ticks: 100, boot_id: "boot-a".into() };
        let same = ProcessIdentity { pid: 10, start_ticks: 100, boot_id: "boot-a".into() };
        let restarted = ProcessIdentity { pid: 10, start_ticks: 101, boot_id: "boot-a".into() };
        let rebooted = ProcessIdentity { pid: 10, start_ticks: 100, boot_id: "boot-b".into() };
        assert!(same_process(&previous, &same));
        assert!(!same_process(&previous, &restarted));
        assert!(!same_process(&previous, &rebooted));
    }

    #[test]
    fn rejects_xray_api_and_test_processes() {
        assert!(!is_xray_proxy_cmdline(b"xray\0api\0bo\0"));
        assert!(!is_xray_proxy_cmdline(b"xray\0run\0-test\0"));
        assert!(!is_xray_proxy_cmdline(b"xray\0run\0-dump\0"));
        assert!(is_xray_proxy_cmdline(b"xray\0run\0-config\0/etc/xray/config.json\0"));
        assert!(is_xray_proxy_cmdline(b"xray\0"));
    }
}
