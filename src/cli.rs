use std::error::Error;

use crate::{maintenance, setup};
use crate::terminal::{command_item, title, separator};

type AppResult<T> = Result<T, Box<dyn Error>>;

const VERSION: &str = env!("XKEEN_SUB_VERSION");

pub fn run() -> AppResult<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.len() == 1 && (args[0] == "--version" || args[0] == "-V") {
        println!("xkeen-sub {VERSION}");
        return Ok(());
    }

    if args.is_empty() || (args.len() == 1 && args[0] == "help") {
        return print_help();
    }

    match args.as_slice() {
        [root, subcommand] if root == "setup" && subcommand == "help" => print_setup_help(),
        [root] if root == "setup" => setup::run_menu(),
        [root, action] if root == "setup" && action == "add" => setup::add::run(),
        [root, action] if root == "setup" && action == "list" => setup::list::run(),
        [root, action] if root == "setup" && action == "del" => setup::del::run(),
        [action] if action == "uxs" => maintenance::update(),
        [action] if action == "reinstall" => maintenance::reinstall(),
        [action] if action == "uninstall" => maintenance::uninstall(),
        _ => {
            print_help()?;
            Ok(())
        }
    }
}

fn print_help() -> AppResult<()> {
    title(&format!("XKEEN-SUB {VERSION} — управление подписками XKeen"));
    println!();
    separator("Использование:");
    command_item("xkeen-sub", "Показать справку");
    command_item("xkeen-sub help", "Показать справку");
    command_item("xkeen-sub setup", "Открыть меню настройки");
    command_item("xkeen-sub setup help", "Справка по настройке");
    command_item("xkeen-sub setup add", "Добавить подписку");
    command_item("xkeen-sub setup list", "Просмотреть подписки");
    command_item("xkeen-sub setup del", "Удалить подписку");
    command_item("xkeen-sub uxs", "Обновить программу");
    command_item("xkeen-sub reinstall", "Выбрать версию и переустановить программу");
    command_item("xkeen-sub uninstall", "Удалить программу");
    Ok(())
}

fn print_setup_help() -> AppResult<()> {
    title("XKEEN-SUB — настройка подписок");
    println!();
    separator("Использование:");
    command_item("xkeen-sub setup", "Открыть меню настройки");
    command_item("xkeen-sub setup add", "Добавить подписку");
    command_item("xkeen-sub setup list", "Просмотреть подписки");
    command_item("xkeen-sub setup del", "Удалить подписку");
    Ok(())
}
