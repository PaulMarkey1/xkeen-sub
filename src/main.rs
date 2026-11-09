mod cli;
mod maintenance;
mod setup;
mod subscriptions;
mod terminal;

fn main() {
    if let Err(error) = cli::run() {
        eprintln!("{}Ошибка: {error}{}", terminal::RED, terminal::RESET);
        std::process::exit(1);
    }
}
