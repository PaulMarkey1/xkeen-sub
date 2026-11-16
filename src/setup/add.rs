use std::error::Error;
use crate::subscription_url::{normalize_subscription_url, SubscriptionUrlError};
use crate::subscriptions::{load_subscriptions, save_subscriptions, Subscription};
use crate::terminal::{error, info, menu_item, read_input, success, title};

type AppResult<T> = Result<T, Box<dyn Error>>;

pub fn run() -> AppResult<bool> {
    let name = loop {
        title("ДОБАВЛЕНИЕ ПОДПИСКИ");
        info("Сначала можно задать имя подписки. Его будет видно в списке.");
        menu_item("1", "Да");
        menu_item("2", "Нет");
        menu_item("0", "Назад");
        menu_item("9", "Выход");
        match read_input("Выберите пункт: ")?.as_str() {
            "1" => {
                let value = read_input("Введите название для подписки: ")?;
                let trimmed = value.trim();
                if trimmed.is_empty() {
                    error("Имя не может быть пустым. Если имя не нужно, выберите «Нет».");
                    continue;
                }
                break Some(trimmed.to_owned());
            }
            "2" => break None,
            "0" => return Ok(false),
            "9" => return Ok(true),
            _ => error("Неизвестный пункт. Введите 1, 2 или 0."),
        }
    };

    let url = loop {
        let value = read_input("Введите URL подписки: ")?;
        match normalize_subscription_url(value.trim()) {
            Ok(valid_url) => break valid_url,
            Err(reason) => error(&format!(
                "Ссылка не подошла: {}. Проверьте адрес и попробуйте ещё раз.",
                reason.add_prompt_message()
            )),
        }
    };

    let mut subscriptions = load_subscriptions()?;
    let next_number = subscriptions.subscriptions.len() + 1;
    let id = format!("sub{next_number}");
    let prefix = match name.as_deref() {
        Some(subscription_name) => format!("{id}--{subscription_name}--"),
        None => format!("{id}--"),
    };

    subscriptions.subscriptions.push(Subscription {
        id,
        name,
        url,
        prefix,
    });

    save_subscriptions(&subscriptions)?;
    success("Готово! Подписка добавлена.");
    Ok(false)
}
