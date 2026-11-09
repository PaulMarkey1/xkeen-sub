use std::error::Error;

use crate::subscriptions::{load_subscriptions, save_subscriptions};
use crate::terminal::{error, info, menu_item, read_input, success, title};

type AppResult<T> = Result<T, Box<dyn Error>>;

pub fn run() -> AppResult<bool> {
    loop {
        let mut store = load_subscriptions()?;

        if store.subscriptions.is_empty() {
            title("XKEEN-SUB — РЕДАКТИРОВАНИЕ ПОДПИСОК");
            info("Список подписок пуст.");
            return Ok(false);
        }

        println!();
        title("XKEEN-SUB — ВЫБОР ПОДПИСКИ");
        for (index, subscription) in store.subscriptions.iter().enumerate() {
            menu_item(&(index + 1).to_string(), &subscription.prefix);
        }

        let return_number = store.subscriptions.len() + 1;
        menu_item(&return_number.to_string(), "Вернуться");
        menu_item("0", "Выход");

        let choice = read_input("Введите номер подписки для редактирования: ")?;
        if choice == "0" {
            return Ok(true);
        }
        if choice == return_number.to_string() {
            return Ok(false);
        }

        let index = match choice.parse::<usize>() {
            Ok(number) if number > 0 && number <= store.subscriptions.len() => number - 1,
            _ => {
                error("Некорректный номер подписки.");
                continue;
            }
        };

        loop {
            let subscription = &store.subscriptions[index];
            println!();
            title("XKEEN-SUB — РЕДАКТИРОВАНИЕ ПОДПИСКИ");
            info(&subscription.prefix);
            menu_item("1", "Изменить название");
            menu_item("2", "Изменить ссылку");
            menu_item("3", "Вернуться");
            menu_item("0", "Выход");

            match read_input("Выберите пункт: ")?.as_str() {
                "1" => {
                    let value = read_input(
                        "Введите новое название (пустая строка — убрать название): ",
                    )?;
                    let name = if value.trim().is_empty() {
                        None
                    } else {
                        Some(value.trim().to_owned())
                    };

                    let subscription = &mut store.subscriptions[index];
                    subscription.name = name;
                    subscription.prefix = make_prefix(&subscription.id, subscription.name.as_deref());
                    save_subscriptions(&store)?;
                    success("Название подписки изменено.");
                }
                "2" => {
                    let url = loop {
                        let value = read_input("Введите новый URL подписки: ")?;
                        match validate_url(value.trim()) {
                            Ok(url) => break url,
                            Err(message) => error(&format!("Некорректный URL: {message}")),
                        }
                    };

                    store.subscriptions[index].url = url;
                    save_subscriptions(&store)?;
                    success("Ссылка подписки изменена.");
                }
                "3" => break,
                "0" => return Ok(true),
                _ => error("Неизвестный пункт. Введите 1, 2, 3 или 0."),
            }
        }
    }
}

fn make_prefix(id: &str, name: Option<&str>) -> String {
    match name {
        Some(name) => format!("{id}--{name}--"),
        None => format!("{id}--"),
    }
}

fn validate_url(value: &str) -> Result<String, &'static str> {
    let parsed = url::Url::parse(value).map_err(|_| "не удалось разобрать адрес")?;
    match parsed.scheme() {
        "http" | "https" => {}
        _ => return Err("разрешены только схемы HTTP и HTTPS"),
    }
    if parsed.host().is_none() {
        return Err("в адресе отсутствует host");
    }
    Ok(parsed.to_string())
}

#[cfg(test)]
mod tests {
    use super::{make_prefix, validate_url};

    #[test]
    fn prefix_includes_name_when_present() {
        assert_eq!(make_prefix("sub2", Some("Germany")), "sub2--Germany--");
    }

    #[test]
    fn prefix_omits_name_when_absent() {
        assert_eq!(make_prefix("sub2", None), "sub2--");
    }

    #[test]
    fn accepts_http_and_https_urls() {
        assert_eq!(
            validate_url("https://example.com/sub").unwrap(),
            "https://example.com/sub"
        );
        assert_eq!(
            validate_url("http://example.com/sub").unwrap(),
            "http://example.com/sub"
        );
    }

    #[test]
    fn rejects_unsupported_url_scheme() {
        assert!(validate_url("ftp://example.com/sub").is_err());
    }

    #[test]
    fn rejects_url_without_host() {
        assert!(validate_url("https:///sub").is_err());
    }
}
