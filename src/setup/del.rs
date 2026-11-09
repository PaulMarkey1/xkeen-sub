use std::error::Error;
use std::io::{self, Write};

use crate::subscriptions::{load_subscriptions, save_subscriptions};
use crate::terminal::{error, success, warning, YELLOW, RESET};

type AppResult<T> = Result<T, Box<dyn Error>>;


pub fn run() -> AppResult<()> {
    let mut store = load_subscriptions()?;

    if store.subscriptions.is_empty() {
        success("Список подписок пуст.");
        return Ok(());
    }

    loop {
        println!("\n{YELLOW}XKEEN-SUB — УДАЛЕНИЕ ПОДПИСКИ{RESET}");
        println!("{YELLOW}──────────────────────────────{RESET}");

        for (index, subscription) in store.subscriptions.iter().enumerate() {
            println!("{}. {}", index + 1, subscription.prefix);
        }

        println!("\n0. Вернуться");

        let choice = read_input(&format!(
            "{YELLOW}Введите номер подписки для удаления: {RESET}"
        ))?;

        if choice == "0" {
            return Ok(());
        }

        let index = match choice.parse::<usize>() {
            Ok(number) if number > 0 && number <= store.subscriptions.len() => number - 1,
            _ => {
                error("Некорректный номер подписки.");
                continue;
            }
        };

        let selected_prefix = store.subscriptions[index].prefix.clone();

        println!(
            "\n{YELLOW}Вы действительно хотите удалить подписку?{RESET}\n{selected_prefix}"
        );
        println!("1. Да");
        println!("2. Нет");

        match read_input(&format!("{YELLOW}Введите номер: {RESET}"))?.as_str() {
            "1" => {
                store.subscriptions.remove(index);

                for (number, subscription) in store.subscriptions.iter_mut().enumerate() {
                    let id = format!("sub{}", number + 1);
                    subscription.id = id.clone();
                    subscription.prefix = match subscription.name.as_deref() {
                        Some(name) => format!("{id}--{name}--"),
                        None => format!("{id}--"),
                    };
                }

                save_subscriptions(&store)?;
                success("Подписка удалена. Нумерация обновлена.");
                return Ok(());
            }
            "2" => {
                warning("Удаление отменено.");
                return Ok(());
            }
            _ => {
                error("Неизвестный пункт. Удаление отменено.");
                return Ok(());
            }
        }
    }
}

fn read_input(prompt: &str) -> AppResult<String> {
    print!("{prompt}");
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;

    Ok(input.trim().to_owned())
}
