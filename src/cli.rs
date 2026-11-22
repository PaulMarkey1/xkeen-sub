use std::error::Error;

use crate::{auto_update, backup, balancers, current_connection, install_config, maintenance, setup, update};
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
        [root, action] if root == "node" && action == "tick" => current_connection::tick(),
        [action] if action == "node" => current_connection::run(),
        [action] if action == "balancers" => balancers::run_menu(),
        [root, rest @ ..] if root == "backup" => backup::run_command(rest),
        [root] if root == "setup" => setup::run_menu(),
        [root] if root == "auto-update" => auto_update::run_menu(),
        [root, action] if root == "auto-update" && action == "tick" => auto_update::tick(),
        [root, action] if root == "setup" && action == "add" => setup::add::run().map(|_| ()),
        [root, action] if root == "setup" && action == "list" => setup::list::run(),
        [root, action] if root == "setup" && action == "del" => setup::del::run().map(|_| ()),
        [root, action] if root == "setup" && action == "edit" => setup::edit::run().map(|_| ()),
        [action] if action == "update" => update::run(None),
        [action, target] if action == "update" => update::run(Some(target)),
        // Internal command used by install.sh before replacing the installed binary.
        [action] if action == "install-check" => install_config::run_install_config_check(),
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
    separator("Команды настройки подписок (все команды начинаются с setup):");
    command_item("setup", "Открыть меню настройки подписок");
    command_item("setup add", "Добавить подписку");
    command_item("setup list", "Просмотреть подписки");
    command_item("setup del", "Удалить подписку");
    command_item("setup edit", "Изменить имя или URL подписки");
    command_item("setup help", "Показать справку по настройке");
    println!();
    command_item("balancers", "Выбрать режим балансировки (пока только просмотр вариантов)");
    command_item("backup", "Настроить и просмотреть резервные копии");
    command_item("  user-backup", "Создать ручную полную точку восстановления");
    command_item("  limit N", "Установить дневной лимит (минимум 5 точек)");
    command_item("  mode auto|unlimited", "Хранить 3 дня или бессрочно");
    command_item("  clear", "Удалить все точки восстановления после подтверждения");
    command_item("  list [ID]", "Просмотреть резервные копии или открыть запись");
    command_item("update", "Обновить все подписки");
    command_item("  subN", "Обновить одну подписку");
    command_item("auto-update", "Настроить автоматическое обновление подписок");
    command_item("node", "Просмотреть и изменить текущее подключение");
    println!();
    command_item("uxs", "Обновить программу");
    command_item("reinstall", "Установить выбранную версию программы");
    command_item("uninstall", "Удалить программу");
    Ok(())
}

fn print_setup_help() -> AppResult<()> {
    title("XKEEN-SUB — настройка подписок");
    println!();
    separator("Использование:");
    command_item("xkeen-sub setup", "Открыть меню настройки (чёрный список — пункт 6, текущий узел — пункт 7)");
    command_item("xkeen-sub setup add", "Добавить подписку");
    command_item("xkeen-sub setup list", "Просмотреть подписки");
    command_item("xkeen-sub setup del", "Удалить подписку");
    command_item("xkeen-sub setup edit", "Редактировать подписку");
    command_item("auto-update", "Настройка автообновления подписок");
    command_item("node", "Просмотреть и изменить текущее подключение");
    Ok(())
}
