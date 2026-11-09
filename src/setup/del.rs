use std::error::Error;
use crate::subscriptions::{load_subscriptions, save_subscriptions};
use crate::terminal::{error, menu_item, read_input, success, title, warning, separator};

type AppResult<T> = Result<T, Box<dyn Error>>;


pub fn run() -> AppResult<()> {
    let mut store = load_subscriptions()?;

    if store.subscriptions.is_empty() {
        success("Список подписок пуст.");
        return Ok(());
    }

    loop {
        println!();
        title("XKEEN-SUB — УДАЛЕНИЕ ПОДПИСКИ");
        separator("──────────────────────────────");

        for (index, subscription) in store.subscriptions.iter().enumerate() {
            menu_item(&(index + 1).to_string(), &subscription.prefix);
        }

        menu_item("0", "Вернуться");

        let choice = read_input("Введите номер подписки для удаления: ")?;

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

        println!();
        title("Вы действительно хотите удалить подписку?");
        println!("{selected_prefix}");
        menu_item("1", "Да");
        menu_item("2", "Нет");

        match read_input("Введите номер: ")?.as_str() {
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

