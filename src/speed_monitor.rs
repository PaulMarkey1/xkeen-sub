use chrono::{Local, TimeZone};
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::process::Command;

use crate::terminal::{error, info, menu_item, read_input, success, title};

type AppResult<T> = Result<T, Box<dyn Error>>;
const CONFIG: &str = "/opt/etc/xkeen-sub/speed-monitor.json";
const LOG: &str = "/opt/etc/xkeen-sub/speed-monitor.log";
const LOCK: &str = "/opt/var/run/xkeen-sub-speed-monitor.lock";
const INTERVAL_SECONDS: i64 = 3600;
const MAX_LOG_LINES: usize = 15_000;

#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    next_run: i64,
}

pub(crate) fn is_enabled() -> bool {
    load().map(|state| state.enabled).unwrap_or(false)
}

pub fn run_menu() -> AppResult<()> {
    loop {
        let state = load()?;
        println!();
        title("XKEEN-SUB — ПОЧАСОВОЙ МОНИТОРИНГ СКОРОСТИ");
        info(&format!("Состояние: {}", if state.enabled { "включён" } else { "выключен" }));
        if state.enabled && state.next_run > 0 {
            info(&format!("Следующая проверка: {}", show_time(state.next_run)));
        }
        menu_item("1", "Включить почасовые проверки");
        menu_item("2", "Выключить почасовые проверки");
        menu_item("3", "Просмотреть журнал");
        menu_item("0", "Назад");
        match read_input("Выберите пункт: ")?.as_str() {
            "1" => enable()?,
            "2" => disable()?,
            "3" => view_log()?,
            "0" => return Ok(()),
            _ => error("Введите цифру от 0 до 3."),
        }
    }
}

fn enable() -> AppResult<()> {
    let mut state = load()?;
    if state.enabled {
        info("Почасовые проверки уже включены.");
        return Ok(());
    }
    state.enabled = true;
    // The first check is scheduled immediately; cron starts it on its next minute tick.
    state.next_run = Local::now().timestamp();
    save(&state)?;
    if let Err(failure) = crate::auto_update::refresh_cron() {
        state.enabled = false;
        state.next_run = 0;
        save(&state)?;
        return Err(failure);
    }
    success("Почасовые проверки включены. Первая проверка начнётся в ближайшую минуту.");
    Ok(())
}

fn disable() -> AppResult<()> {
    let mut state = load()?;
    if !state.enabled {
        info("Почасовые проверки уже выключены.");
        return Ok(());
    }
    state.enabled = false;
    state.next_run = 0;
    save(&state)?;
    crate::auto_update::refresh_cron()?;
    success("Почасовые проверки выключены. Уже начавшийся замер будет завершён.");
    Ok(())
}

pub(crate) fn tick() -> AppResult<()> {
    let _lock = match crate::update_lock::FileLock::acquire(LOCK)? {
        Some(lock) => lock,
        None => return Ok(()),
    };
    let mut state = load()?;
    let now = Local::now().timestamp();
    if !state.enabled || state.next_run > now {
        return Ok(());
    }

    // Advance the hourly slot before starting the child process so a failure
    // cannot cause cron to launch the same measurement every minute.
    let mut next_run = state.next_run;
    while next_run <= now {
        next_run = next_run.saturating_add(INTERVAL_SECONDS);
        if next_run == i64::MAX { break; }
    }
    state.next_run = next_run;
    save(&state)?;

    let started_at = Local::now();
    let mut entry = format!(
        "\n========== ПРОВЕРКА {} ==========\n",
        started_at.format("%Y-%m-%d %H:%M:%S")
    );
    match std::env::current_exe() {
        Ok(executable) => match Command::new(executable).arg("test").output() {
            Ok(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                if !stdout.trim().is_empty() {
                    entry.push_str(stdout.trim_end());
                    entry.push('\n');
                }
                if !stderr.trim().is_empty() {
                    entry.push_str("ОШИБКИ:\n");
                    entry.push_str(stderr.trim_end());
                    entry.push('\n');
                }
                if output.status.success() {
                    entry.push_str("Статус проверки: завершена.\n");
                } else {
                    entry.push_str(&format!("Статус проверки: ошибка (код {}).\n", output.status));
                }
            }
            Err(failure) => entry.push_str(&format!("Не удалось запустить тест подключений: {failure}\n")),
        },
        Err(failure) => entry.push_str(&format!("Не удалось определить путь к xkeen-sub: {failure}\n")),
    }
    entry.push_str(&format!(
        "Следующая проверка: {}\n",
        show_time(state.next_run)
    ));
    entry.push_str("========================================\n");
    append_log(&entry)?;
    Ok(())
}

fn load() -> AppResult<State> {
    if !Path::new(CONFIG).exists() {
        return Ok(State::default());
    }
    Ok(serde_json::from_slice(&fs::read(CONFIG)?)?)
}

fn save(state: &State) -> AppResult<()> {
    fs::create_dir_all("/opt/etc/xkeen-sub")?;
    let temp = format!("{CONFIG}.tmp.{}", std::process::id());
    let mut file = fs::File::create(&temp)?;
    file.write_all(&serde_json::to_vec_pretty(state)?)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    fs::rename(temp, CONFIG)?;
    Ok(())
}

fn append_log(entry: &str) -> AppResult<()> {
    fs::create_dir_all("/opt/etc/xkeen-sub")?;
    let mut file = OpenOptions::new().create(true).append(true).open(LOG)?;
    file.write_all(entry.as_bytes())?;
    file.sync_all()?;

    let content = fs::read_to_string(LOG)?;
    let lines: Vec<&str> = content.lines().collect();
    if lines.len() > MAX_LOG_LINES {
        let retained = lines[lines.len() - MAX_LOG_LINES..].join("\n");
        let temp = format!("{LOG}.tmp.{}", std::process::id());
        fs::write(&temp, format!("{retained}\n"))?;
        fs::rename(temp, LOG)?;
    }
    Ok(())
}

fn view_log() -> AppResult<()> {
    title("ЖУРНАЛ ПОЧАСОВОГО МОНИТОРИНГА");
    match fs::read_to_string(LOG) {
        Ok(content) if content.trim().is_empty() => info("Журнал пока пуст."),
        Ok(content) => print!("{content}"),
        Err(failure) if failure.kind() == std::io::ErrorKind::NotFound => info("Журнал пока пуст."),
        Err(failure) => return Err(format!("Не удалось прочитать журнал мониторинга: {failure}").into()),
    }
    let _ = read_input("\nНажмите Enter, чтобы вернуться...");
    Ok(())
}

fn show_time(timestamp: i64) -> String {
    Local.timestamp_opt(timestamp, 0).single()
        .map(|value| value.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_else(|| "не определено".to_owned())
}
