mod cli;
mod config;
mod maintenance;
mod setup;
mod subscriptions;
mod terminal;

fn main() {
    if let Err(error) = cli::run() {
        terminal::error(&format!("Ошибка: {error}"));
        std::process::exit(1);
    }
}
