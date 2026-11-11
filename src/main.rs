mod cli;
mod config;
mod maintenance;
mod setup;
mod subscriptions;
mod terminal;
#[allow(dead_code)]
mod xray_test;

fn main() {
    if let Err(error) = cli::run() {
        terminal::error(&format!("Ошибка: {error}"));
        std::process::exit(1);
    }
}
