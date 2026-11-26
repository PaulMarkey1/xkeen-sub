mod auto_update;
mod update_lock;
mod backup;
mod blacklist;
mod balancer;
mod balancers;
mod balancer_mode_switch;
mod cli;
mod current_connection;
mod install_config;
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
mod xray_probe;
mod speed_test;
mod speed_monitor;
mod test;
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

fn configure_timezone() {
    // Keenetic stores its active POSIX timezone in /etc/TZ (also exposed as
    // /tmp/TZ). Entware's libc/chrono must receive it explicitly: /etc/localtime
    // on Keenetic may point to this text file rather than a TZif database.
    for path in ["/etc/TZ", "/tmp/TZ"] {
        if let Ok(value) = std::fs::read_to_string(path) {
            let value = value.trim();
            if !value.is_empty() {
                std::env::set_var("TZ", value);
                return;
            }
        }
    }
    if std::path::Path::new("/opt/etc/localtime").exists() {
        std::env::set_var("TZ", ":/opt/etc/localtime");
    }
}

fn main() {
    configure_timezone();
    if let Err(error) = cli::run() {
        terminal::error(&format!("Ошибка: {error}"));
        std::process::exit(1);
    }
}
