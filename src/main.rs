mod backup;
mod balancer;
mod cli;
mod install;
mod maintenance;
mod observatory;
mod outbound_compare;
mod setup;
mod subscriptions;
mod system;
mod subscription_download;
mod subscription_delete;
mod subscription_rename;
mod subscription_identity;
mod subscription_parser;
mod subscription_url;
mod terminal;
mod temp_counter;
mod xray_test;
mod update;
mod update_baseline;
mod update_configuration;
mod update_config;
mod update_plan;
mod update_report;
mod update_subscription;
mod update_state;
mod update_merge;
mod update_apply;

fn main() {
    if let Err(error) = cli::run() {
        terminal::error(&format!("Ошибка: {error}"));
        std::process::exit(1);
    }
}
