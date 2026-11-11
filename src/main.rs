mod cli;
mod config;
mod maintenance;
mod outbound_compare;
mod setup;
mod subscriptions;
mod subscription_download;
mod terminal;
#[allow(dead_code)]
mod xray_test;

fn main() {
    if let Err(error) = cli::run() {
        terminal::error(&format!("Ошибка: {error}"));
        std::process::exit(1);
    }
}
