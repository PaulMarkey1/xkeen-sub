use std::error::Error;

use crate::subscriptions::load_subscriptions;
use crate::terminal::{success, GREEN, CYAN, RESET};

type AppResult<T> = Result<T, Box<dyn Error>>;

pub fn run() -> AppResult<()> {
    let store = load_subscriptions()?;

    if store.subscriptions.is_empty() {
        success("Список подписок пуст.");
        return Ok(());
    }

    for (index, subscription) in store.subscriptions.iter().enumerate() {
        if index > 0 {
            println!();
        }

        println!("{GREEN}╭────────────────────────────{RESET}");
        print_field("ID:", &subscription.id, "      ");
        print_field("Имя:", subscription.name.as_deref().unwrap_or("—"), "     ");
        print_field("Префикс:", &subscription.prefix, " ");
        print_label("Ссылка:");
        println!("{GREEN}│{RESET} {}", subscription.url);
        println!("{GREEN}╰────────────────────────────{RESET}");
    }

    Ok(())
}

fn print_field(label: &str, value: &str, spacing: &str) {
    println!("{GREEN}│{RESET} {CYAN}{label}{RESET}{spacing}{value}");
}

fn print_label(label: &str) {
    println!("{GREEN}│{RESET} {CYAN}{label}{RESET}");
}
