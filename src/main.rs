mod cli;
mod config;
mod install;
mod maintenance;
#[allow(dead_code)]
mod outbound_compare;
mod setup;
mod subscriptions;
mod subscription_download;
mod terminal;
#[allow(dead_code)]
mod xray_test;
#[allow(dead_code)]
mod update;

fn main() {
    if let Err(error) = cli::run() {
        terminal::error(&format!("Ошибка: {error}"));
        std::process::exit(1);
    }
}
