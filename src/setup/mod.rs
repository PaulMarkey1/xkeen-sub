pub mod add;
pub mod del;
pub mod list;

use std::error::Error;
use std::io::{self, Write};

type AppResult<T> = Result<T, Box<dyn Error>>;

pub fn run_menu() -> AppResult<()> {
    loop {
        println!(
            "\nXKEEN-SUB — НАСТРОЙКА\n             ────────────────────────\n             1. Добавить подписку\n             2. Просмотреть подписки\n             3. Удалить подписку\n             0. Выход"
        );

        print!("Выберите пункт: ");
        io::stdout().flush()?;

        let mut choice = String::new();
        io::stdin().read_line(&mut choice)?;

        match choice.trim() {
            "1" => add::run()?,
            "2" => list::run()?,
            "3" => del::run()?,
            "0" => return Ok(()),
            _ => println!("Неизвестный пункт меню. Введите цифру от 0 до 3."),
        }
    }
}
