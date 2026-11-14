use std::error::Error;

use crate::{backup, install, maintenance, setup, update};
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
        [root] if root == "backup" => backup::run(),
        [root, rest @ ..] if root == "backup" => backup::run_command(rest),
        [root] if root == "setup" => setup::run_menu(),
        [root, action] if root == "setup" && action == "add" => setup::add::run(),
        [root, action] if root == "setup" && action == "list" => setup::list::run(),
        [root, action] if root == "setup" && action == "del" => setup::del::run(),
        [root, action] if root == "setup" && action == "edit" => setup::edit::run().map(|_| ()),
        [action] if action == "update" => update::run(None),
        [action, target] if action == "update" => update::run(Some(target)),
        // Internal command used by install.sh before replacing the installed binary.
        [action] if action == "install-check" => install::ensure_observatory(),
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
    command_item("help", "Показать справку");
    command_item("--version", "Показать номер версии");
    println!();
    command_item("setup", "Настройка подписок");
    command_item("  add", "Добавить подписку");
    command_item("  list", "Просмотреть подписки");
    command_item("  del", "Удалить подписку");
    command_item("  edit", "Редактировать подписку");
    command_item("  help", "Справка по настройке");
    println!();
    command_item("backup", "Настройки и управление резервными копиями");
    command_item("  days N", "Установить срок хранения (минимум 3 дня)");
    command_item("  limit N", "Установить дневной лимит (минимум 10 записей)");
    command_item("  mode auto|unlimited", "Выбрать режим хранения");
    command_item("  list [ID]", "Просмотреть резервные копии или открыть запись");
    command_item("update", "Контрольный запуск обновления всех подписок");
    command_item("  subN", "Контрольный запуск одной подписки");
    println!();
    command_item("uxs", "Обновить программу");
    command_item("reinstall", "Выбрать версию и переустановить программу");
    command_item("uninstall", "Удалить программу");
    Ok(())
}

fn print_setup_help() -> AppResult<()> {
    title("XKEEN-SUB — настройка подписок");
    println!();
    separator("Использование:");
    command_item("setup", "Открыть меню настройки");
    command_item("  add", "Добавить подписку");
    command_item("  list", "Просмотреть подписки");
    command_item("  del", "Удалить подписку");
    command_item("  edit", "Редактировать подписку");
    Ok(())
}
