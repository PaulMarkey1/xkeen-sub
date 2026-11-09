pub mod add;
pub mod del;
pub mod list;

use std::error::Error;
use crate::terminal::{error, menu_item, read_input, separator, title};

type AppResult<T> = Result<T, Box<dyn Error>>;

pub fn run_menu() -> AppResult<()> {
    loop {
        println!();
        title("XKEEN-SUB — НАСТРОЙКА");
        separator("────────────────────────");
        menu_item("1", "Добавить подписку");
        menu_item("2", "Просмотреть подписки");
        menu_item("3", "Удалить подписку");
        menu_item("0", "Выход");

        let choice = read_input("Выберите пункт: ")?;

        match choice.as_str() {
            "1" => add::run()?,
            "2" => list::run()?,
            "3" => del::run()?,
            "0" => return Ok(()),
            _ => error("Неизвестный пункт меню. Введите цифру от 0 до 3."),
        }
    }
}
