mod cli;
mod setup;
mod subscriptions;

fn main() {
    if let Err(error) = cli::run() {
        eprintln!("Ошибка: {error}");
        std::process::exit(1);
    }
}
