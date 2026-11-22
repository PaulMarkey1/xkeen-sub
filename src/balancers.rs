use std::error::Error;

use crate::terminal::{error, menu_item, read_input, separator, title};

type AppResult<T> = Result<T, Box<dyn Error>>;

/// Displays the balancing-mode menu. Mode selection is intentionally
/// informational until the corresponding configuration workflows are implemented.
pub fn run_menu() -> AppResult<()> {
    loop {
        println!();
        title("XKEEN-SUB — РЕЖИМ БАЛАНСИРОВКИ");
        separator("────────────────────────────────");
        menu_item("1", "Простой — текущая схема xkeen-sub");
        menu_item("2", "Продвинутый — три балансировщика (пока не реализован)");
        menu_item("3", "Пользовательский — собственная схема (пока не реализован)");
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
