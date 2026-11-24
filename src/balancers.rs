use std::error::Error;
use std::fs;
use std::path::Path;

use crate::balancer::{detect_balancing_mode, BalancingMode};
use crate::terminal::{error, menu_item, read_input, separator, title};
use crate::update_config::{CONFIG_DIR, ROUTING_FILE};

type AppResult<T> = Result<T, Box<dyn Error>>;

fn current_mode_label() -> String {
    let path = Path::new(CONFIG_DIR).join(ROUTING_FILE);
    let source = match fs::read_to_string(&path) {
        Ok(source) => source,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return "Не настроен".to_owned();
        }
        Err(err) => {
            return format!("Не удалось определить (ошибка чтения: {err})");
        }
    };

    let document: serde_json::Value = match json5::from_str(&source) {
        Ok(document) => document,
        Err(err) => {
            return format!("Не удалось определить (ошибка разбора конфигурации: {err})");
        }
    };

    match detect_balancing_mode(&document) {
        BalancingMode::NotConfigured => "Не настроен".to_owned(),
        BalancingMode::Simple => "Простой".to_owned(),
        BalancingMode::Advanced => "Продвинутый".to_owned(),
        BalancingMode::Custom => "Пользовательский".to_owned(),
        BalancingMode::IncompleteAdvanced => "Неполный продвинутый".to_owned(),
        BalancingMode::Ambiguous => "Неоднозначная конфигурация".to_owned(),
    }
}

/// Displays the balancing-mode menu. Mode selection remains informational
/// until the corresponding configuration workflows are implemented.
pub fn run_menu() -> AppResult<()> {
    loop {
        println!();
        title("XKEEN-SUB — РЕЖИМ БАЛАНСИРОВКИ");
        println!("Текущий режим: {}", current_mode_label());
        separator("────────────────────────────────");
        menu_item("1", "Простой");
        menu_item("2", "Продвинутый");
        menu_item("3", "Пользовательский");
        menu_item("0", "Назад");

        let choice = read_input("Выберите режим: ")?;

        match choice.as_str() {
            "1" | "2" | "3" => {
                println!("Выбор режима пока не применяется. Настройка балансировщиков не изменена.");
            }
            "0" => return Ok(()),
            _ => error("Неизвестный пункт меню. Введите цифру от 0 до 3."),
        }
    }
}
