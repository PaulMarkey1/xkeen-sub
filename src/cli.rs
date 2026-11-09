use std::error::Error;

use crate::{maintenance, setup};

type AppResult<T> = Result<T, Box<dyn Error>>;

pub fn run() -> AppResult<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

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
    println!(
        "xkeen-sub — управление подписками XKeen\n\n\
         Использование:\n\
           xkeen-sub                 Показать справку\n\
           xkeen-sub help            Показать справку\n\
           xkeen-sub setup           Открыть меню настройки\n\
           xkeen-sub setup help      Справка по настройке\n\
           xkeen-sub setup add       Добавить подписку\n\
           xkeen-sub setup list      Просмотреть подписки\n\
           xkeen-sub setup del       Удалить подписку\n\
           xkeen-sub uxs             Обновить программу\n\
           xkeen-sub uninstall      Удалить программу"
    );
    Ok(())
}

fn print_setup_help() -> AppResult<()> {
    println!(
        "xkeen-sub setup — настройка подписок\n\n\
         Использование:\n\
           xkeen-sub setup           Открыть меню настройки\n\
           xkeen-sub setup add       Добавить подписку\n\
           xkeen-sub setup list      Просмотреть подписки\n\
           xkeen-sub setup del       Удалить подписку"
    );
    Ok(())
}
