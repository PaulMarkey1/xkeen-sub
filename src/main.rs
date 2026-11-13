mod balancer;
mod cli;
mod config;
mod install;
mod maintenance;
mod observatory;
mod outbound_compare;
mod setup;
mod subscriptions;
mod subscription_download;
mod terminal;
mod xray_test;
mod update;
mod update_merge;

fn main() {
    if let Err(error) = cli::run() {
        terminal::error(&format!("Ошибка: {error}"));
        std::process::exit(1);
    }
}
