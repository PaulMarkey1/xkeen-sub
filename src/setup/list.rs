use std::error::Error;

use crate::subscriptions::load_subscriptions;

type AppResult<T> = Result<T, Box<dyn Error>>;

pub fn run() -> AppResult<()> {
    let store = load_subscriptions()?;

    if store.subscriptions.is_empty() {
        println!("Список подписок пуст.");
        return Ok(());
    }

    let rows: Vec<[String; 4]> = store
        .subscriptions
        .iter()
        .map(|subscription| {
            [
                subscription.id.clone(),
                subscription.name.as_deref().unwrap_or("—").to_owned(),
                subscription.prefix.clone(),
                subscription.url.clone(),
            ]
        })
        .collect();

    let headers = ["ID", "Имя", "Префикс", "Ссылка"];
    let mut widths = headers.map(str::len);

    for row in &rows {
        for (index, value) in row.iter().enumerate() {
            widths[index] = widths[index].max(value.len());
        }
    }

    print_row(&headers.map(str::to_owned), &widths);
    println!(
        "{}",
        widths
            .iter()
            .map(|width| "-".repeat(*width))
            .collect::<Vec<_>>()
            .join("-+-")
    );

    for row in rows {
        print_row(&row, &widths);
    }

    Ok(())
}

fn print_row(values: &[String; 4], widths: &[usize; 4]) {
    println!(
        "{:<width0$} | {:<width1$} | {:<width2$} | {:<width3$}",
        values[0],
        values[1],
        values[2],
        values[3],
        width0 = widths[0],
        width1 = widths[1],
        width2 = widths[2],
        width3 = widths[3],
    );
}
