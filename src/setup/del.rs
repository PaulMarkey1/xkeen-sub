use std::error::Error;
use crate::subscription_identity::SubscriptionIdentity;
use crate::subscriptions::{load_subscriptions, save_subscriptions};
use crate::terminal::{error, info, menu_item, read_input, success, title, separator};

type AppResult<T> = Result<T, Box<dyn Error>>;

pub fn run() -> AppResult<bool> {
    let mut store = load_subscriptions()?;

    if store.subscriptions.is_empty() {
        info("Подписок пока нет — удалять нечего.");
        return Ok(false);
    }

    loop {
        println!();
        title("XKEEN-SUB — УДАЛЕНИЕ ПОДПИСКИ");
        separator("──────────────────────────────");

        for (index, subscription) in store.subscriptions.iter().enumerate() {
            menu_item(&(index + 1).to_string(), &subscription.prefix);
        }

        menu_item("0", "Назад");
        menu_item("9", "Выход");

        let choice = read_input("Введите номер подписки для удаления: ")?;

        if choice == "0" {
            return Ok(false);
        }
        if choice == "9" {
            return Ok(true);
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
        title("УДАЛИТЬ ЭТУ ПОДПИСКУ?");
        info(&selected_prefix);
        menu_item("1", "Да");
        menu_item("2", "Нет");

        match read_input("Введите номер: ")?.as_str() {
            "1" => {
                store.subscriptions.remove(index);

                for (number, subscription) in store.subscriptions.iter_mut().enumerate() {
                    let identity = SubscriptionIdentity::from_number(
                        number + 1,
                        subscription.name.as_deref(),
                    );
                    subscription.id = identity.id;
                    subscription.prefix = identity.prefix;
                }

                save_subscriptions(&store)?;
                success("Подписка удалена. Номера оставшихся подписок обновлены.");
                return Ok(false);
            }
            "2" => {
                info("Подписка оставлена без изменений.");
                return Ok(false);
            }
            _ => {
                error("Неизвестный пункт. Удаление отменено.");
                return Ok(false);
            }
        }
    }
}
