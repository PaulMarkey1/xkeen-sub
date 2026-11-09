use std::error::Error;
use std::io::{self, Write};

use crate::subscriptions::{load_subscriptions, save_subscriptions, Subscription};
use crate::terminal::{error, success};

type AppResult<T> = Result<T, Box<dyn Error>>;

pub fn run() -> AppResult<()> {
    let name = loop {
        println!("Добавить название подписки?\n1 Да\n2 Нет\n0 Вернуться");
        match read_input("Выберите пункт: ")?.as_str() {
            "1" => {
                let value = read_input("Введите название для подписки: ")?;
                let trimmed = value.trim();
                if trimmed.is_empty() {
                    error("Название не может быть пустым. Для подписки без названия выберите «Нет».");
                    continue;
                }
                break Some(trimmed.to_owned());
            }
            "2" => break None,
            "0" => return Ok(()),
            _ => error("Неизвестный пункт. Введите 1, 2 или 0."),
        }
    };

    let url = loop {
        let value = read_input("Введите URL подписки: ")?;
        match validate_url(value.trim()) {
            Ok(valid_url) => break valid_url,
            Err(message) => error(&format!("Некорректный URL: {message}")),
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
    success("Подписка добавлена.");
    Ok(())
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

fn read_input(prompt: &str) -> AppResult<String> {
    print!("{prompt}");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    Ok(input.trim_end_matches(['\r', '\n']).to_owned())
}
