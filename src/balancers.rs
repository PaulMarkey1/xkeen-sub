use std::error::Error;

use crate::balancer::BalancingMode;
use crate::terminal::{error, info, menu_item, read_input, separator, title};

type AppResult<T> = Result<T, Box<dyn Error>>;

pub fn run_menu() -> AppResult<()> {
    loop {
        println!();
        title("XKEEN-SUB — РЕЖИМ БАЛАНСИРОВКИ");
        println!("Текущий режим: {}", crate::balancer_mode_switch::current_mode_label());
        separator("────────────────────────────────");
        menu_item("1", "Простой");
        menu_item("2", "Продвинутый");
        menu_item("3", "Пользовательский");
        menu_item("0", "Назад");

        match read_input("Выберите режим: ")?.as_str() {
            "1" => crate::balancer_mode_switch::switch_mode(BalancingMode::Simple)?,
            "2" => crate::balancer_mode_switch::switch_mode(BalancingMode::Advanced)?,
            "3" => {
                info("Пользовательский режим включается автоматически, когда конфигурация балансировщиков отличается от шаблонов Simple и Advanced.");
                info("Чтобы вернуться к режиму xkeen-sub, выберите «Простой» или «Продвинутый».");
            }
            "0" => return Ok(()),
            _ => error("Неизвестный пункт меню. Введите цифру от 0 до 3."),
        }
    }
}
