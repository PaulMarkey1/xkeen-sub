use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::outbound_compare::{compare_subscription, ComparisonReport};
use crate::subscription_download::download_subscription;
use crate::subscriptions::{load_subscriptions, Subscription};
use crate::terminal::{error, info, success, title, warning};

type AppResult<T> = Result<T, Box<dyn Error>>;

const BASELINE_DIR: &str = "/opt/etc/xkeen-sub";

#[derive(Debug)]
pub struct PreparedSubscription {
    pub subscription_id: String,
    pub original_body: Vec<u8>,
    pub baseline_path: PathBuf,
    pub comparison: ComparisonReport,
}

#[derive(Debug, Default)]
pub struct UpdatePreparation {
    pub prepared: Vec<PreparedSubscription>,
    pub downloaded: usize,
    pub failed: Vec<(String, String)>,
}

/// Downloads and compares subscriptions in sequence. It does not change the
/// live XKeen configuration or write baselines; those operations belong to
/// the later application stage after backup and rollback are agreed.
pub fn prepare_updates(target_id: Option<&str>) -> AppResult<UpdatePreparation> {
    let store = load_subscriptions()?;

    if store.subscriptions.is_empty() {
        info("Список подписок пуст. Добавьте подписку через xkeen-sub setup add.");
        return Ok(UpdatePreparation::default());
    }

    let selected: Vec<&Subscription> = match target_id {
        Some(id) => {
            let subscription = store
                .subscriptions
                .iter()
                .find(|subscription| subscription.id == id)
                .ok_or_else(|| format!("Подписка {id} не найдена."))?;
            vec![subscription]
        }
        None => store.subscriptions.iter().collect(),
    };

    let mut preparation = UpdatePreparation::default();

    for subscription in selected {
        println!();
        title(&format!(
            "Подготовка обновления: {}",
            subscription
                .name
                .as_deref()
                .filter(|name| !name.is_empty())
                .unwrap_or(&subscription.id)
        ));

        info("Загрузка подписки...");
        let downloaded = match download_subscription(subscription) {
            Ok(bytes) => {
                preparation.downloaded += 1;
                success("Загрузка завершена.");
                bytes
            }
            Err(download_error) => {
                record_failure(&mut preparation, subscription, download_error.to_string());
                continue;
            }
        };

        info("Чтение baseline...");
        let baseline_path = match baseline_path(subscription) {
            Ok(path) => path,
            Err(path_error) => {
                record_failure(&mut preparation, subscription, path_error.to_string());
                continue;
            }
        };

        let previous = match fs::read(&baseline_path) {
            Ok(bytes) => {
                success(&format!("Baseline найден: {}", baseline_path.display()));
                Some(bytes)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                warning("Baseline отсутствует. Все распознанные подключения будут считаться новыми.");
                None
            }
            Err(read_error) => {
                record_failure(
                    &mut preparation,
                    subscription,
                    format!(
                        "Не удалось прочитать baseline {}: {read_error}",
                        baseline_path.display()
                    ),
                );
                continue;
            }
        };

        info("Сравнение подключений...");
        let comparison = match compare_subscription(previous.as_deref(), &downloaded) {
            Ok(report) => {
                success("Сравнение завершено.");
                report
            }
            Err(compare_error) => {
                record_failure(
                    &mut preparation,
                    subscription,
                    format!("Не удалось обработать подписку: {compare_error}"),
                );
                continue;
            }
        };

        print_report(subscription, &comparison);
        preparation.prepared.push(PreparedSubscription {
            subscription_id: subscription.id.clone(),
            original_body: downloaded,
            baseline_path,
            comparison,
        });
    }

    if preparation.downloaded == 0 {
        println!();
        info("Ни одна подписка не ответила. Проверка интернет-соединения...");
        if has_internet_connection() {
            warning("Интернет-соединение доступно, но серверы подписок не ответили.");
        } else {
            error("Отсутствует интернет-соединение.");
        }
        warning("Конфигурация XKeen не изменена.");
    }

    println!();
    title("Итог подготовки");
    info(&format!("Успешно загружено: {}", preparation.downloaded));
    info(&format!("Подготовлено отчётов: {}", preparation.prepared.len()));
    if !preparation.failed.is_empty() {
        warning(&format!("Ошибок обработки: {}", preparation.failed.len()));
    }
    info("Изменения в XKeen и baseline не записывались.");

    Ok(preparation)
}

fn record_failure(
    preparation: &mut UpdatePreparation,
    subscription: &Subscription,
    message: String,
) {
    error(&format!("Ошибка подписки {}: {message}", subscription.id));
    warning("Подготовка этой подписки пропущена. Прежняя конфигурация сохранена.");
    preparation.failed.push((subscription.id.clone(), message));
}

fn baseline_path(subscription: &Subscription) -> AppResult<PathBuf> {
    let has_name = subscription
        .name
        .as_deref()
        .map(|name| !name.is_empty())
        .unwrap_or(false);

    let filename = if has_name {
        subscription
            .prefix
            .strip_suffix("--")
            .unwrap_or(&subscription.prefix)
            .to_owned()
    } else {
        subscription.prefix.clone()
    };

    if filename.is_empty()
        || filename == "."
        || filename == ".."
        || filename.contains('/')
        || filename.contains('\\')
    {
        return Err(format!(
            "Недопустимый префикс подписки {} для имени baseline.",
            subscription.id
        )
        .into());
    }

    Ok(Path::new(BASELINE_DIR).join(format!("{filename}.json")))
}

fn print_report(subscription: &Subscription, report: &ComparisonReport) {
    let name = subscription
        .name
        .as_deref()
        .filter(|name| !name.is_empty())
        .unwrap_or(&subscription.id);

    println!();
    title(&format!("Отчёт: {name}"));

    if report.added.is_empty()
        && report.removed.is_empty()
        && report.changed.is_empty()
        && report.unchanged == 0
    {
        info("Распознанных подключений нет.");
    }

    if !report.added.is_empty() {
        success("Добавлены:");
        for outbound in &report.added {
            success(&format!("  + {}", outbound.tag));
        }
    }

    if !report.removed.is_empty() {
        error("Удалены:");
        for outbound in &report.removed {
            error(&format!("  - {}", outbound.tag));
        }
    }

    if !report.changed.is_empty() {
        warning("Изменены:");
        for outbound in &report.changed {
            println!("  {}", outbound.tag);
            for change in &outbound.changes {
                println!("    {CYAN}{}{RESET}:", change.path);
                println!("      Было: {}", display_value(change.previous.as_ref()));
                println!("      Стало: {}", display_value(change.current.as_ref()));
            }
        }
    }

    info(&format!("Без изменений: {}", report.unchanged));

    if report.unrecognized_current > 0 {
        warning(&format!(
            "Нераспознанных подключений в новой загрузке: {}",
            report.unrecognized_current
        ));
    }
    if report.unrecognized_previous > 0 {
        warning(&format!(
            "Нераспознанных подключений в baseline: {}",
            report.unrecognized_previous
        ));
    }
    if report.identical_duplicates_removed > 0 {
        warning(&format!(
            "Одинаковых дубликатов Tag устранено: {}",
            report.identical_duplicates_removed
        ));
    }

    if report.added.is_empty()
        && report.removed.is_empty()
        && report.changed.is_empty()
        && report.unrecognized_current == 0
        && report.unrecognized_previous == 0
    {
        success("Изменений нет. Подписка актуальна.");
    } else if report.added.is_empty() && report.removed.is_empty() && report.changed.is_empty() {
        warning("Изменений среди распознанных подключений нет; часть данных не распознана.");
    }
}

const CYAN: &str = "\x1b[36m";
const RESET: &str = "\x1b[0m";

fn display_value(value: Option<&serde_json::Value>) -> String {
    match value {
        Some(value) => serde_json::to_string(value).unwrap_or_else(|_| value.to_string()),
        None => "<отсутствует>".to_owned(),
    }
}

fn has_internet_connection() -> bool {
    const URLS: [&str; 3] = [
        "https://connectivitycheck.gstatic.com/generate_204",
        "https://www.cloudflare.com/",
        "https://www.google.com/",
    ];

    if command_exists("curl") {
        return URLS.iter().any(|url| {
            Command::new("curl")
                .args([
                    "--location",
                    "--silent",
                    "--output",
                    "/dev/null",
                    "--connect-timeout",
                    "5",
                    "--max-time",
                    "10",
                    url,
                ])
                .status()
                .map(|status| status.success())
                .unwrap_or(false)
        });
    }

    if command_exists("wget") {
        return URLS.iter().any(|url| {
            Command::new("wget")
                .args(["--quiet", "--spider", "--timeout=5", "--tries=1", url])
                .status()
                .map(|status| status.success())
                .unwrap_or(false)
        });
    }

    false
}

fn command_exists(command: &str) -> bool {
    Command::new("sh")
        .args(["-c", &format!("command -v {command} >/dev/null 2>&1")])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::baseline_path;
    use crate::subscriptions::Subscription;
    use std::path::Path;

    fn subscription(name: Option<&str>, prefix: &str) -> Subscription {
        Subscription {
            id: "sub1".to_owned(),
            name: name.map(str::to_owned),
            url: "https://example.com/sub".to_owned(),
            prefix: prefix.to_owned(),
        }
    }

    #[test]
    fn named_subscription_uses_prefix_without_final_separator() {
        let path = baseline_path(&subscription(Some("Germany"), "sub1--Germany--")).unwrap();
        assert_eq!(path, Path::new("/opt/etc/xkeen-sub/sub1--Germany.json"));
    }

    #[test]
    fn unnamed_subscription_keeps_trailing_separator_in_filename() {
        let path = baseline_path(&subscription(None, "sub1--")).unwrap();
        assert_eq!(path, Path::new("/opt/etc/xkeen-sub/sub1--.json"));
    }

    #[test]
    fn path_separators_in_prefix_are_rejected() {
        assert!(baseline_path(&subscription(None, "../outside--")).is_err());
        assert!(baseline_path(&subscription(None, "nested/name--")).is_err());
    }
}
