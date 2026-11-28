use chrono::{DateTime, Local, LocalResult, NaiveDate, NaiveDateTime, NaiveTime, TimeZone};
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::Command;

use crate::terminal::{error, info, menu_item, read_input, success, title, warning};

type AppResult<T> = Result<T, Box<dyn Error>>;
const CONFIG: &str = "/opt/etc/xkeen-sub/auto-update.json";
const MARKER: &str = "# xkeen-sub-auto-update";
const CRON: &str = "* * * * * /opt/sbin/xkeen-sub auto-update tick # xkeen-sub-auto-update";
const MAX_LATE_SECONDS: i64 = 3600;
const LOG: &str = "/opt/etc/xkeen-sub/auto-update.log";
const MAX_LOG_LINES: usize = 100;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Target { All, Subscription(String) }

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Schedule {
    Daily { time: String },
    Interval { seconds: u64 },
    Calendar { days: u32, hours: u64, minutes: u64, time: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct Job {
    id: u64,
    enabled: bool,
    target: Target,
    schedule: Schedule,
    next_run: i64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Config {
    #[serde(default)] jobs: Vec<Job>,
    #[serde(default = "first_id")] next_id: u64,
}
fn first_id() -> u64 { 1 }

pub fn run_menu() -> AppResult<()> {
    loop {
        println!();
        title("XKEEN-SUB — АВТОМАТИЧЕСКОЕ ОБНОВЛЕНИЕ");
        menu_item("1", "Просмотреть расписания");
        menu_item("2", "Добавить расписание");
        menu_item("3", "Изменить расписание");
        menu_item("4", "Включить или выключить расписание");
        menu_item("5", "Удалить расписание");
        menu_item("6", "Журнал автообновления");
        menu_item("0", "Назад");
        match read_input("Выберите пункт: ")?.as_str() {
            "1" => list_jobs()?,
            "2" => add_job()?,
            "3" => edit_job()?,
            "4" => toggle_job()?,
            "5" => delete_job()?,
            "6" => view_log()?,
            "0" => return Ok(()),
            _ => error("Введите цифру от 0 до 6."),
        }
    }
}

fn is_summary_log_entry(message: &str) -> bool {
    message.trim_start().starts_with("ИТОГ:")
}

pub(crate) fn log_event(message: &str) -> AppResult<()> {
    // BackupSession sends every technical event through this hook. Keep those
    // details exclusively in backup/.../log.txt; the bounded auto-update log
    // is reserved for concise subscription outcomes.
    if !is_summary_log_entry(message) {
        return Ok(());
    }

    fs::create_dir_all("/opt/etc/xkeen-sub")?;
    let message = message.replace('\r', " ").replace('\n', " ");
    let line = format!("[{}] {}\n", Local::now().format("%Y-%m-%d %H:%M:%S"), message.trim());
    let mut lines: Vec<String> = match fs::read_to_string(LOG) {
        Ok(content) => content.lines()
            .filter(|entry| entry.contains("] ИТОГ:"))
            .map(str::to_owned)
            .collect(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error.into()),
    };
    lines.push(line.trim_end().to_owned());
    trim_log_lines(&mut lines);
    let temp = format!("{LOG}.tmp.{}", std::process::id());
    let mut content = lines.join("\n");
    content.push('\n');
    fs::write(&temp, content)?;
    fs::rename(temp, LOG)?;
    Ok(())
}

fn trim_log_lines(lines: &mut Vec<String>) {
    if lines.len() > MAX_LOG_LINES {
        let excess = lines.len() - MAX_LOG_LINES;
        lines.drain(0..excess);
    }
}

fn view_log() -> AppResult<()> {
    title("ЖУРНАЛ АВТООБНОВЛЕНИЯ");
    match fs::read_to_string(LOG) {
        Ok(content) if content.trim().is_empty() => info("Журнал пока пуст."),
        Ok(content) => {
            for line in content.lines() {
                println!("{line}");
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => info("Журнал пока пуст."),
        Err(error) => return Err(format!("Не удалось прочитать журнал автообновления: {error}").into()),
    }
    let _ = read_input("Нажмите Enter, чтобы вернуться...");
    Ok(())
}

pub fn tick() -> AppResult<()> {
    let _tick = match crate::update_lock::FileLock::acquire("/opt/var/run/xkeen-sub-auto-update-tick.lock")? {
        Some(lock) => lock,
        None => return Ok(()),
    };
    // The same minute cron also handles the expiry and restart detection
    // of a manually selected Xray node, even when no update schedule is enabled.
    crate::current_connection::tick()?;

    let initial = load()?;
    let due: Vec<u64> = initial.jobs.iter()
        .filter(|job| job.enabled && job.next_run <= Local::now().timestamp())
        .map(|job| job.id).collect();

    for id in due {
        // Do not consume a due occurrence while a manual update is running.
        let _update = match crate::update_lock::FileLock::acquire("/opt/var/run/xkeen-sub-update.lock")? {
            Some(lock) => lock,
            None => return Ok(()),
        };
        // Reload before each change so a concurrent setup-menu edit is not
        // overwritten by a stale configuration snapshot.
        let mut current = load()?;
        let Some(index) = current.jobs.iter().position(|job| job.id == id && job.enabled) else { continue };
        let current_now = Local::now().timestamp();
        if current.jobs[index].next_run > current_now { continue; }

        if current_now.saturating_sub(current.jobs[index].next_run) > MAX_LATE_SECONDS {
            // The router was offline or the scheduler was blocked past this slot.
            // Skip the missed occurrence and retain the original interval/calendar phase.
            current.jobs[index].next_run = next_occurrence(&current.jobs[index], current_now)?;
            save(&current)?;
            let _ = log_event(&format!("Расписание №{id}: пропущен запуск, опоздание превысило допустимый час."));
            continue;
        }

        let schedule_snapshot = current.jobs[index].schedule.clone();
        let target_snapshot = current.jobs[index].target.clone();
        let next_run_snapshot = next_occurrence(&current.jobs[index], current_now)?;
        current.jobs[index].next_run = next_run_snapshot;
        save(&current)?;

        let target = match &target_snapshot {
            Target::All => None,
            Target::Subscription(id) => Some(id.as_str()),
        };
        // The existing update pipeline owns all subscription processing and logging.
        if let Err(failure) = crate::update::run_auto_locked(target) {
            eprintln!("xkeen-sub auto-update: {failure}");
        }

        // If the update itself ran across one or more scheduled slots, skip
        // those missed occurrences. Do not overwrite a schedule edited meanwhile.
        let finished_at = Local::now().timestamp();
        let mut latest = load()?;
        if let Some(job) = latest.jobs.iter_mut().find(|job| {
            job.id == id && job.enabled && job.schedule == schedule_snapshot
                && job.target == target_snapshot && job.next_run == next_run_snapshot
        }) {
            if job.next_run <= finished_at {
                job.next_run = next_occurrence(job, finished_at)?;
                save(&latest)?;
            }
        }
    }
    crate::speed_monitor::tick()?;
    Ok(())
}

fn load() -> AppResult<Config> {
    if !Path::new(CONFIG).exists() { return Ok(Config::default()); }
    Ok(serde_json::from_slice(&fs::read(CONFIG)?)?)
}
fn save(config: &Config) -> AppResult<()> {
    fs::create_dir_all("/opt/etc/xkeen-sub")?;
    let temp = format!("{CONFIG}.tmp");
    let bytes = serde_json::to_vec_pretty(config)?;
    let mut file = fs::File::create(&temp)?;
    file.write_all(&bytes)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    fs::rename(temp, CONFIG)?;
    sync_cron(config.jobs.iter().any(|job| job.enabled))
}

pub(crate) fn refresh_cron() -> AppResult<()> {
    let config = load()?;
    sync_cron(config.jobs.iter().any(|job| job.enabled))
}

fn sync_cron(enabled: bool) -> AppResult<()> {
    // Keep the existing single cron entry while either scheduled updates or
    // a temporary manual node selection needs periodic processing.
    let enabled = enabled || crate::current_connection::has_pending_selection() || crate::speed_monitor::is_enabled();
    if !crate::system::command_exists("crontab") {
        if enabled { return Err("Не найдена команда crontab. Установите cron в Entware.".into()); }
        return Ok(());
    }
    let output = Command::new("crontab").arg("-l").output()?;
    let current = if output.status.success() {
        String::from_utf8(output.stdout)?
    } else {
        let message = String::from_utf8_lossy(&output.stderr).to_lowercase();
        if message.contains("no crontab") || message.contains("нет crontab")
            || (message.contains("can't open") && message.contains("root"))
            || message.contains("no such file or directory") { String::new() }
        else { return Err(format!("Не удалось прочитать crontab: {}", message.trim()).into()); }
    };
    let mut lines: Vec<String> = current.lines().filter(|line| !line.contains(MARKER)).map(str::to_owned).collect();
    if enabled {
        ensure_cron()?;
        lines.push(CRON.to_owned());
    }
    let temp = format!("/opt/etc/xkeen-sub/crontab.tmp.{}", std::process::id());
    let mut content = lines.join("\n");
    if !content.is_empty() { content.push('\n'); }
    fs::write(&temp, content)?;
    let result = Command::new("crontab").arg(&temp).output();
    let _ = fs::remove_file(&temp);
    let result = result?;
    if !result.status.success() {
        return Err(format!("Не удалось записать crontab: {}", String::from_utf8_lossy(&result.stderr).trim()).into());
    }
    Ok(())
}

fn ensure_cron() -> AppResult<()> {
    if !crate::system::command_exists("cron") && !crate::system::command_exists("crond") {
        return Err("Не найден демон cron. Установите пакет cron в Entware.".into());
    }
    if cron_running() { return Ok(()); }
    let mut scripts = Vec::new();
    if let Ok(entries) = fs::read_dir("/opt/etc/init.d") {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_lowercase();
            if name.starts_with("s10") && name.contains("cron") && entry.path().is_file() {
                scripts.push(entry.path());
            }
        }
    }
    scripts.sort();
    let script = scripts.first().ok_or("Служба cron не запущена: не найден сценарий S10cron в /opt/etc/init.d.")?;
    let status = Command::new(script).arg("start").status()?;
    if !status.success() || !cron_running() { return Err("Не удалось запустить службу cron.".into()); }
    Ok(())
}
fn is_cron_process_name(name: &str) -> bool {
    matches!(name.trim(), "cron" | "crond")
}
fn cron_running() -> bool {
    fs::read_dir("/proc").ok().into_iter().flatten().flatten().any(|entry| {
        let pid = entry.file_name();
        let Some(pid) = pid.to_str().filter(|s| s.chars().all(|c| c.is_ascii_digit())) else { return false };
        matches!(fs::read_to_string(format!("/proc/{pid}/comm")), Ok(s) if is_cron_process_name(&s))
    })
}

fn list_jobs() -> AppResult<()> {
    let config = load()?;
    title("РАСПИСАНИЯ АВТООБНОВЛЕНИЯ");
    if config.jobs.is_empty() { info("Расписаний пока нет."); return Ok(()); }
    let subscriptions = crate::subscriptions::load_subscriptions()?;
    for job in &config.jobs {
        info(&format!("№{} — {}", job.id, target_label(&job.target, &subscriptions.subscriptions)));
        info(&format!("  {}; {}; следующий запуск: {}",
            describe(&job.schedule),
            if job.enabled { "включено" } else { "выключено" },
            show_time(job.next_run)?));
    }
    Ok(())
}

fn target_label(target: &Target, subscriptions: &[crate::subscriptions::Subscription]) -> String {
    match target {
        Target::All => "все подписки".to_owned(),
        Target::Subscription(id) => match subscriptions.iter().find(|sub| sub.id == *id) {
            Some(sub) => format!("{} ({})", id, sub.name.as_deref().unwrap_or("без имени")),
            None => format!("{} (подписка не найдена)", id),
        },
    }
}

fn normalize_job_ids(config: &mut Config) {
    for (index, job) in config.jobs.iter_mut().enumerate() {
        job.id = index as u64 + 1;
    }
    config.next_id = config.jobs.len() as u64 + 1;
}
fn add_job() -> AppResult<()> {
    let mut config = load()?;
    normalize_job_ids(&mut config);
    let target = choose_target()?;
    let (schedule, next_run) = read_schedule()?;
    let id = config.next_id;
    config.next_id = id.checked_add(1).ok_or("Исчерпаны номера расписаний.")?;
    config.jobs.push(Job { id, enabled: true, target, schedule, next_run });
    save(&config)?;
    success(&format!("Расписание №{id} добавлено и включено."));
    Ok(())
}
fn edit_job() -> AppResult<()> {
    let mut config = load()?;
    let Some(i) = choose_job(&config)? else { return Ok(()) };
    let target = choose_target()?;
    let (schedule, next_run) = read_schedule()?;
    config.jobs[i].target = target;
    config.jobs[i].schedule = schedule;
    config.jobs[i].next_run = next_run;
    save(&config)?;
    success("Расписание изменено.");
    Ok(())
}
fn toggle_job() -> AppResult<()> {
    let mut config = load()?;
    let Some(i) = choose_job(&config)? else { return Ok(()) };
    config.jobs[i].enabled = !config.jobs[i].enabled;
    save(&config)?;
    success(if config.jobs[i].enabled { "Расписание включено." } else { "Расписание выключено." });
    Ok(())
}
fn delete_job() -> AppResult<()> {
    let config = load()?;
    let Some(i) = choose_job(&config)? else { return Ok(()) };
    let selected_job = config.jobs[i].clone();
    let selected_id = selected_job.id;

    // tick identifies due schedules by ID. Lock only after the user chooses,
    // then reload to avoid deleting a different job after a concurrent update.
    let _tick = match crate::update_lock::FileLock::acquire("/opt/var/run/xkeen-sub-auto-update-tick.lock")? {
        Some(lock) => lock,
        None => {
            warning("Сейчас выполняется обновление по расписанию. Повторите удаление чуть позже.");
            return Ok(());
        }
    };
    let mut latest = load()?;
    let Some(index) = latest.jobs.iter().position(|job| job.id == selected_id) else {
        warning("Это расписание уже изменилось. Откройте список ещё раз.");
        return Ok(());
    };
    if latest.jobs[index] != selected_job {
        warning("Параметры расписания изменились. Откройте список ещё раз и повторите удаление.");
        return Ok(());
    }
    latest.jobs.remove(index);
    normalize_job_ids(&mut latest);
    save(&latest)?;
    success(&format!("Расписание №{selected_id} удалено. Оставшиеся расписания перенумерованы."));
    Ok(())
}
fn choose_job(config: &Config) -> AppResult<Option<usize>> {
    if config.jobs.is_empty() { info("Расписаний пока нет."); return Ok(None); }
    let subscriptions = crate::subscriptions::load_subscriptions()?;
    loop {
        title("ВЫБОР РАСПИСАНИЯ");
        for (i, job) in config.jobs.iter().enumerate() {
            menu_item(&(i + 1).to_string(), &format!("№{} — {}", job.id, target_label(&job.target, &subscriptions.subscriptions)));
            info(&format!("  {}; {}; запуск: {}",
                describe(&job.schedule),
                if job.enabled { "включено" } else { "выключено" },
                show_time(job.next_run)?));
        }
        menu_item("0", "Назад");
        let value = read_input("Выберите расписание по номеру в списке: ")?;
        if value == "0" { return Ok(None); }
        match value.parse::<usize>() {
            Ok(n) if n > 0 && n <= config.jobs.len() => {
                let job = &config.jobs[n - 1];
                title(&format!("РАСПИСАНИЕ №{}", job.id));
                info(&format!("Обновление: {}", target_label(&job.target, &subscriptions.subscriptions)));
                info(&format!("Периодичность: {}", describe(&job.schedule)));
                info(&format!("Состояние: {}", if job.enabled { "включено" } else { "выключено" }));
                info(&format!("Следующий запуск: {}", show_time(job.next_run)?));
                return Ok(Some(n - 1));
            }
            _ => error("Расписания с таким номером нет. Попробуйте ещё раз."),
        }
    }
}
fn choose_target() -> AppResult<Target> {
    loop {
        title("ЧТО ОБНОВЛЯТЬ?");
        menu_item("1", "Все подписки");
        menu_item("2", "Одну подписку");
        match read_input("Выберите вариант: ")?.as_str() {
            "1" => return Ok(Target::All),
            "2" => {
                let store = crate::subscriptions::load_subscriptions()?;
                if store.subscriptions.is_empty() { warning("Подписок пока нет. Сначала добавьте подписку."); continue; }
                for (i, sub) in store.subscriptions.iter().enumerate() {
                    menu_item(&(i + 1).to_string(), &format!("{} — {}", sub.id, sub.name.as_deref().unwrap_or("без имени")));
                }
                let value = read_input("Номер подписки: ")?;
                match value.parse::<usize>() {
                    Ok(n) if n > 0 && n <= store.subscriptions.len() => {
                        return Ok(Target::Subscription(store.subscriptions[n - 1].id.clone()));
                    }
                    _ => error("Подписки с таким номером нет. Попробуйте ещё раз."),
                }
            }
            _ => error("Введите 1 или 2."),
        }
    }
}

fn read_schedule() -> AppResult<(Schedule, i64)> {
    loop {
        title("ТИП РАСПИСАНИЯ");
        menu_item("1", "Каждый день в определённое время");
        menu_item("2", "Через интервал (дни, часы, минуты)");
        menu_item("3", "Через интервал с заданным временем запуска");
        let choice = read_input("Выберите тип: ")?;
        let result: AppResult<(Schedule, i64)> = (|| {
            match choice.as_str() {
                "1" => {
                    let time = read_time("Время запуска (ЧЧ:ММ): ")?;
                    Ok((Schedule::Daily { time: time.clone() }, next_daily(&time, Local::now().timestamp())?))
                }
                "2" => {
                    let days = read_number("Интервал — дней: ")?;
                    let hours = read_number("Интервал — часов (0–23): ")?;
                    let minutes = read_number("Интервал — минут (0–59): ")?;
                    if hours >= 24 || minutes >= 60 { return Err("Часы должны быть от 0 до 23, минуты — от 0 до 59.".into()); }
                    let seconds = days.checked_mul(86400).and_then(|v| hours.checked_mul(3600).and_then(|h| v.checked_add(h)))
                        .and_then(|v| minutes.checked_mul(60).and_then(|m| v.checked_add(m)))
                        .ok_or("Слишком большой интервал.")?;
                    if seconds == 0 { return Err("Интервал должен быть больше нуля.".into()); }
                    if seconds > i64::MAX as u64 { return Err("Интервал слишком велик.".into()); }
                    let first_time = read_time("Время первого запуска (ЧЧ:ММ): ")?;
                    // The first occurrence is the next local time matching the input.
                    // If today's time has already passed, it naturally falls on tomorrow.
                    let first = next_daily(&first_time, Local::now().timestamp())?;
                    Ok((Schedule::Interval { seconds }, first))
                }
                "3" => {
                    let days = u32::try_from(read_number("Интервал — календарных дней: ")?).ok().filter(|v| *v > 0)
                        .ok_or("Количество дней должно быть не меньше 1.")?;
                    let hours = read_number("Дополнительно часов (0–23): ")?;
                    let minutes = read_number("Дополнительно минут (0–59): ")?;
                    if hours >= 24 || minutes >= 60 { return Err("Часы должны быть от 0 до 23, минуты — от 0 до 59.".into()); }
                    let time = read_time("Время запуска (ЧЧ:ММ): ")?;
                    let date = read_date("Дата первого запуска (ГГГГ-ММ-ДД): ")?;
                    let offset = hours.checked_mul(60).and_then(|v| v.checked_add(minutes)).ok_or("Слишком большое смещение.")?;
                    let offset = i64::try_from(offset).map_err(|_| "Слишком большое смещение.")?;
                    let first = resolve_local(date.and_time(NaiveTime::parse_from_str(&time, "%H:%M")?) + chrono::Duration::minutes(offset))?.timestamp();
                    if first <= Local::now().timestamp() { return Err("Дата первого запуска должна быть в будущем.".into()); }
                    Ok((Schedule::Calendar { days, hours, minutes, time }, first))
                }
                _ => Err("Выберите один из трёх вариантов.".into()),
            }
        })();
        match result { Ok(value) => return Ok(value), Err(e) => error(&e.to_string()) }
    }
}
fn read_number(prompt: &str) -> AppResult<u64> {
    read_input(prompt)?.parse::<u64>().map_err(|_| "Введите целое неотрицательное число.".into())
}
fn read_time(prompt: &str) -> AppResult<String> {
    let value = read_input(prompt)?;
    NaiveTime::parse_from_str(&value, "%H:%M").map_err(|_| "Используйте формат ЧЧ:ММ, например 17:30.")?;
    Ok(value)
}
fn read_date(prompt: &str) -> AppResult<NaiveDate> {
    NaiveDate::parse_from_str(&read_input(prompt)?, "%Y-%m-%d").map_err(|_| "Используйте формат ГГГГ-ММ-ДД.".into())
}

fn next_occurrence(job: &Job, now: i64) -> AppResult<i64> {
    match &job.schedule {
        Schedule::Daily { time } => advance_calendar(job.next_run, 1, 0, 0, time, now),
        Schedule::Calendar { days, hours, minutes, time } => advance_calendar(job.next_run, *days, *hours, *minutes, time, now),
        Schedule::Interval { seconds } => {
            let period = i64::try_from(*seconds).map_err(|_| "Интервал слишком велик.")?;
            if period <= 0 { return Err("В конфигурации указан нулевой интервал.".into()); }
            let steps = now.saturating_sub(job.next_run) / period + 1;
            job.next_run.checked_add(steps.saturating_mul(period)).ok_or_else(|| "Дата следующего запуска вне диапазона.".into())
        }
    }
}
fn advance_calendar(previous: i64, days: u32, hours: u64, minutes: u64, time: &str, now: i64) -> AppResult<i64> {
    if days == 0 { return Err("Календарный интервал должен быть не меньше одного дня.".into()); }
    let clock = NaiveTime::parse_from_str(time, "%H:%M")?;
    let offset = hours.checked_mul(60).and_then(|v| v.checked_add(minutes)).ok_or("Смещение слишком велико.")?;
    let offset = i64::try_from(offset).map_err(|_| "Смещение слишком велико.")?;
    let mut previous_local = local_time(previous)?.naive_local();
    loop {
        let date = previous_local.date().checked_add_signed(chrono::Duration::days(days as i64)).ok_or("Дата вне диапазона.")?;
        let candidate = resolve_local(date.and_time(clock) + chrono::Duration::minutes(offset))?.timestamp();
        if candidate > now { return Ok(candidate); }
        previous_local = local_time(candidate)?.naive_local();
    }
}
fn next_daily(time: &str, now: i64) -> AppResult<i64> {
    let clock = NaiveTime::parse_from_str(time, "%H:%M")?;
    let today = local_time(now)?.date_naive();
    let candidate = resolve_local(today.and_time(clock))?.timestamp();
    if candidate > now { return Ok(candidate); }
    let tomorrow = today.checked_add_signed(chrono::Duration::days(1)).ok_or("Дата вне диапазона.")?;
    Ok(resolve_local(tomorrow.and_time(clock))?.timestamp())
}
fn resolve_local(value: NaiveDateTime) -> AppResult<DateTime<Local>> {
    match Local.from_local_datetime(&value) {
        LocalResult::Single(v) => Ok(v),
        LocalResult::Ambiguous(a, b) => Ok(if a.timestamp() <= b.timestamp() { a } else { b }),
        LocalResult::None => {
            for n in 1..=180 {
                match Local.from_local_datetime(&(value + chrono::Duration::minutes(n))) {
                    LocalResult::Single(v) => return Ok(v),
                    LocalResult::Ambiguous(a, b) => return Ok(if a.timestamp() <= b.timestamp() { a } else { b }),
                    LocalResult::None => {}
                }
            }
            Err("Не удалось определить местное время при переходе часового пояса.".into())
        }
    }
}
fn local_time(ts: i64) -> AppResult<DateTime<Local>> {
    Local.timestamp_opt(ts, 0).single().ok_or_else(|| "Некорректная дата.".into())
}
fn show_time(ts: i64) -> AppResult<String> { Ok(local_time(ts)?.format("%Y-%m-%d %H:%M").to_string()) }
fn describe(s: &Schedule) -> String {
    match s {
        Schedule::Daily { time } => format!("ежедневно в {time}"),
        Schedule::Interval { seconds } => {
            let days = seconds / 86400;
            let hours = seconds % 86400 / 3600;
            let minutes = seconds % 3600 / 60;
            let mut parts = Vec::new();
            if days > 0 { parts.push(format!("{days} дн.")); }
            if hours > 0 { parts.push(format!("{hours} ч.")); }
            if minutes > 0 || parts.is_empty() { parts.push(format!("{minutes} мин.")); }
            format!("каждые {}", parts.join(" "))
        },
        Schedule::Calendar { days, hours, minutes, time } => format!("каждые {days} календарных дн., +{hours} ч. {minutes} мин., от {time}"),
    }
}

pub(crate) fn remap_subscription_ids(removed: &str, mapping: &[(String, String)]) -> AppResult<()> {
    if !Path::new(CONFIG).exists() { return Ok(()); }
    let mut config = load()?;
    let old_len = config.jobs.len();
    config.jobs.retain(|job| !matches!(&job.target, Target::Subscription(id) if id == removed));
    for job in &mut config.jobs {
        if let Target::Subscription(id) = &mut job.target {
            if let Some((_, new_id)) = mapping.iter().find(|(old, _)| old == id) { *id = new_id.clone(); }
        }
    }
    if old_len != config.jobs.len() || !mapping.is_empty() { save(&config)?; }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn ts(s: &str) -> i64 {
        let dt = NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap();
        resolve_local(dt).unwrap().timestamp()
    }
    #[test]
    fn recognizes_both_entware_cron_daemon_names() {
        assert!(is_cron_process_name("cron"));
        assert!(is_cron_process_name("crond"));
        assert!(!is_cron_process_name("cron-helper"));
        assert!(!is_cron_process_name("xray"));
    }

    #[test]
    fn auto_update_log_accepts_only_summary_entries() {
        assert!(is_summary_log_entry("ИТОГ: sub1 — обновлена."));
        assert!(is_summary_log_entry("  ИТОГ: sub2 — ошибка загрузки."));
        assert!(!is_summary_log_entry("DOWNLOAD: sub1: получено 123 байт."));
        assert!(!is_summary_log_entry("Запуск: все подписки."));
    }

    #[test]
    fn auto_update_log_keeps_only_existing_summary_entries() {
        let entries = [
            "[2026-11-21 10:00:00] DOWNLOAD: raw details",
            "[2026-11-21 10:01:00] ИТОГ: sub1 — обновлена.",
        ];
        let retained: Vec<&str> = entries.into_iter()
            .filter(|line| line.contains("] ИТОГ:"))
            .collect();
        assert_eq!(retained.len(), 1);
        assert!(retained[0].contains("sub1 — обновлена"));
    }

    #[test]
    fn auto_update_log_keeps_only_the_latest_100_lines() {
        let mut lines: Vec<String> = (1..=101).map(|n| n.to_string()).collect();
        trim_log_lines(&mut lines);
        assert_eq!(lines.len(), 100);
        assert_eq!(lines.first().map(String::as_str), Some("2"));
        assert_eq!(lines.last().map(String::as_str), Some("101"));
    }

    #[test]
    fn auto_update_log_does_not_change_short_logs() {
        let mut lines = vec!["first".to_owned(), "second".to_owned()];
        trim_log_lines(&mut lines);
        assert_eq!(lines, vec!["first", "second"]);
    }

    #[test]
    fn deleting_schedule_compacts_ids_and_resets_next_id() {
        let mut config = Config {
            jobs: vec![
                Job { id: 4, enabled: true, target: Target::All, schedule: Schedule::Daily { time: "06:00".into() }, next_run: 100 },
                Job { id: 9, enabled: false, target: Target::All, schedule: Schedule::Daily { time: "12:00".into() }, next_run: 200 },
                Job { id: 300, enabled: true, target: Target::All, schedule: Schedule::Daily { time: "18:00".into() }, next_run: 300 },
            ],
            next_id: 301,
        };
        config.jobs.remove(1);
        normalize_job_ids(&mut config);
        assert_eq!(config.jobs.iter().map(|job| job.id).collect::<Vec<_>>(), vec![1, 2]);
        assert_eq!(config.next_id, 3);
        assert_eq!(config.jobs.iter().map(|job| job.next_run).collect::<Vec<_>>(), vec![100, 300]);
    }

    #[test]
    fn adding_after_compaction_uses_the_next_sequential_id() {
        let mut config = Config {
            jobs: vec![Job { id: 87, enabled: true, target: Target::All, schedule: Schedule::Daily { time: "06:00".into() }, next_run: 100 }],
            next_id: 88,
        };
        normalize_job_ids(&mut config);
        let new_id = config.next_id;
        config.next_id += 1;
        assert_eq!(new_id, 2);
        assert_eq!(config.next_id, 3);
    }

    #[test]
    fn interval_keeps_anchor_after_late_execution() {
        let job = Job { id: 1, enabled: true, target: Target::All, schedule: Schedule::Interval { seconds: 21600 }, next_run: ts("2026-11-19 00:00") };
        assert_eq!(show_time(next_occurrence(&job, ts("2026-11-19 06:20")).unwrap()).unwrap(), "2026-11-19 12:00");
    }
    #[test]
    fn calendar_days_cross_month_boundary() {
        let first = ts("2026-11-29 17:00");
        let job = Job { id: 1, enabled: true, target: Target::All, schedule: Schedule::Calendar { days: 2, hours: 0, minutes: 0, time: "17:00".into() }, next_run: first };
        assert_eq!(show_time(next_occurrence(&job, first).unwrap()).unwrap(), "2026-12-01 17:00");
    }
    #[test]
    fn daily_uses_next_local_day_when_time_has_passed() {
        assert_eq!(show_time(next_daily("17:00", ts("2026-11-19 18:00")).unwrap()).unwrap(), "2026-11-20 17:00");
    }
    #[test]
    fn interval_first_run_uses_tomorrow_when_input_time_has_passed() {
        assert_eq!(show_time(next_daily("03:00", ts("2026-11-19 22:00")).unwrap()).unwrap(), "2026-11-20 03:00");
    }
    #[test]
    fn interval_first_run_uses_today_when_input_time_is_upcoming() {
        assert_eq!(show_time(next_daily("23:00", ts("2026-11-19 22:00")).unwrap()).unwrap(), "2026-11-19 23:00");
    }
    #[test]
    fn missed_interval_slots_are_skipped_without_shifting_the_anchor() {
        let job = Job { id: 1, enabled: true, target: Target::All, schedule: Schedule::Interval { seconds: 21600 }, next_run: ts("2026-11-19 06:00") };
        assert_eq!(show_time(next_occurrence(&job, ts("2026-11-19 13:00")).unwrap()).unwrap(), "2026-11-19 18:00");
    }
}