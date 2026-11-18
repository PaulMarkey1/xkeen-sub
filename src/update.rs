use crate::backup::BackupSession;
use crate::update_merge::subscription_id_number;
use crate::subscriptions::{load_subscriptions, Subscription};
use crate::system::command_exists;
use crate::terminal::{error, info, title, warning};
use crate::update_subscription::prepare_subscriptions;
use crate::update_configuration::update_configuration;
use crate::update_baseline::apply_baseline_only;
use crate::update_report::{print_baseline_plan, print_subscription_summary};

use std::error::Error;
use std::process::Command;
use std::time::{Duration, Instant};

type AppResult<T> = Result<T, Box<dyn Error>>;

/// Downloads and analyses subscriptions, prepares documents, validates the
/// complete Xray configuration, backs up originals, then applies the plan with
/// automatic rollback and XKeen state verification.
pub fn run(target_id: Option<&str>) -> AppResult<()> {
    let started_at = Instant::now();
    let mut backup = BackupSession::start("Обновление подписок")?;
    backup.event("UPDATE", "Загрузка и анализ подписок.")?;

    let result = run_inner(target_id, &mut backup);
    if let Err(ref error) = result {
        if backup.is_running() {
            backup.set_outcome("ОШИБКА", &format!("Выполнение update прервано: {error}"), true);
        }
    } else if backup.is_running() {
        backup.set_outcome(
            "ЗАВЕРШЕНО",
            "Обновление завершено без изменения рабочих файлов.",
            true,
        );
    }

    let log_result = backup.finish();
    println!();
    info(&format!("Резервная запись: {}.", backup.id()));
    info(&format!("Время выполнения: {}.", format_elapsed(started_at.elapsed())));
    if let Err(error) = log_result {
        return Err(format!("Не удалось завершить журнал резервной записи: {error}").into());
    }
    result
}

fn run_inner(target_id: Option<&str>, backup: &mut BackupSession) -> AppResult<()> {
    println!();
    title("XKEEN-SUB — ОБНОВЛЕНИЕ ПОДПИСОК");
    warning("Перед применением будет создана проверенная резервная копия; при сбое предусмотрен откат.");
    backup.event("UPDATE", "Начата загрузка списка подписок.")?;

    let store = load_subscriptions()?;
    backup.event("UPDATE", &format!("Загружен список подписок: {}.", store.subscriptions.len()))?;
    if store.subscriptions.is_empty() {
        info("Подписок пока нет. Добавьте подписку, чтобы запустить обновление.");
        return Ok(());
    }

    let mut selected: Vec<&Subscription> = match target_id {
        Some(id) => vec![store.subscriptions.iter()
            .find(|item| item.id == id)
            .ok_or_else(|| format!("Подписка {id} не найдена."))?],
        None => store.subscriptions.iter().collect(),
    };
    selected.sort_by_key(|item| subscription_id_number(&item.id).unwrap_or(u64::MAX));
    backup.set_subscriptions(selected.iter().map(|item| {
        match item.name.as_deref() {
            Some(name) if !name.trim().is_empty() => format!("{} ({name})", item.id),
            _ => item.id.clone(),
        }
    }).collect());

    let state = prepare_subscriptions(&selected, backup)?;

    if state.downloaded == 0 {
        println!();
        info("Не удалось получить данные подписок. Проверяем подключение к интернету...");
        if has_internet_connection() {
            warning("Интернет работает, но серверы подписок не ответили. Проверьте ссылки подписок или попробуйте позже.");
        } else {
            error("Нет подключения к интернету. Проверьте соединение и повторите попытку.");
        }
        print_subscription_summary(&state);
        backup.event("ERROR", "Не удалось загрузить ни одну подписку; применение заблокировано.")?;
        backup.set_outcome("ОШИБКА ЗАГРУЗКИ", "Ни одна подписка не была успешно загружена.", true);
        warning("Обновление остановлено. Файлы роутера не менялись.");
        return Ok(());
    }
    if state.processed == 0 {
        print_subscription_summary(&state);
        print_baseline_plan(&state);
        backup.event("ERROR", "Ни одна подписка не прошла обработку; дальнейшая подготовка прекращена.")?;
        backup.set_outcome("ОШИБКА ОБРАБОТКИ", "Ни одна подписка не прошла обработку.", true);
        warning("Не удалось обработать ни одну подписку. Конфигурация роутера осталась без изменений.");
        return Ok(());
    }

    if state.prepared.is_empty() {
        return apply_baseline_only(&state, backup);
    }

    update_configuration(&store.subscriptions, &state, backup)

}

fn format_elapsed(duration: Duration) -> String {
    let total_seconds = duration.as_secs();
    let hours = total_seconds / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;

    let mut parts = Vec::new();
    if hours > 0 {
        parts.push(format!("{hours} {}", russian_plural(hours, "час", "часа", "часов")));
    }
    if minutes > 0 {
        parts.push(format!("{minutes} {}", russian_plural(minutes, "минута", "минуты", "минут")));
    }
    if seconds > 0 || parts.is_empty() {
        parts.push(format!("{seconds} {}", russian_plural(seconds, "секунда", "секунды", "секунд")));
    }
    parts.join(" ")
}

fn russian_plural(value: u64, one: &'static str, few: &'static str, many: &'static str) -> &'static str {
    let last_two = value % 100;
    let last = value % 10;
    if (11..=14).contains(&last_two) {
        // The returned literals have static lifetime.
        many
    } else {
        match last {
            1 => one,
            2..=4 => few,
            _ => many,
        }
    }
}

#[cfg(test)]
mod elapsed_time_tests {
    use super::format_elapsed;
    use std::time::Duration;

    #[test]
    fn formats_only_non_zero_units() {
        assert_eq!(format_elapsed(Duration::from_secs(5)), "5 секунд");
        assert_eq!(format_elapsed(Duration::from_secs(65)), "1 минута 5 секунд");
        assert_eq!(format_elapsed(Duration::from_secs(3600)), "1 час");
        assert_eq!(format_elapsed(Duration::from_secs(3661)), "1 час 1 минута 1 секунда");
    }

    #[test]
    fn formats_russian_plural_forms() {
        assert_eq!(format_elapsed(Duration::from_secs(2)), "2 секунды");
        assert_eq!(format_elapsed(Duration::from_secs(11)), "11 секунд");
        assert_eq!(format_elapsed(Duration::from_secs(21)), "21 секунда");
    }

    #[test]
    fn zero_duration_still_shows_seconds() {
        assert_eq!(format_elapsed(Duration::ZERO), "0 секунд");
    }
}


fn has_internet_connection() -> bool {
    const URLS: [&str; 2] = ["https://www.cloudflare.com/", "https://www.google.com/"];
    if command_exists("curl") {
        return URLS.iter().any(|url| Command::new("curl")
            .args(["--location", "--silent", "--output", "/dev/null", "--connect-timeout", "5", "--max-time", "10", url])
            .status().map(|s| s.success()).unwrap_or(false));
    }
    if command_exists("wget") {
        return URLS.iter().any(|url| Command::new("wget")
            .args(["--quiet", "--spider", "--timeout=5", "--tries=1", url])
            .status().map(|s| s.success()).unwrap_or(false));
    }
    false
}


