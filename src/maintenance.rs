use std::error::Error;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

type AppResult<T> = Result<T, Box<dyn Error>>;

const CONFIG_DIR: &str = "/opt/etc/xkeen-sub";
const SUBSCRIPTIONS_FILE: &str = "/opt/etc/xkeen-sub/sub.json";
const CONFIG_FILE: &str = "/opt/etc/xkeen-sub/config.json";
const REPO: &str = "PaulMarkey1/xkeen-sub";

pub fn update() -> AppResult<()> {
    let executable = std::env::current_exe()?;
    let asset = detect_asset()?;
    let url = format!("https://github.com/{REPO}/releases/latest/download/{asset}");
    let temporary = temporary_path(&executable, "update");

    println!("Проверка последнего релиза...");
    let download_result = download(&url, &temporary);
    if let Err(error) = download_result {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }

    if !temporary.is_file() || fs::metadata(&temporary)?.len() == 0 {
        let _ = fs::remove_file(&temporary);
        return Err("Загруженный файл пуст или отсутствует.".into());
    }

    let mut permissions = fs::metadata(&temporary)?.permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&temporary, permissions)?;

    let check = Command::new(&temporary).arg("--help").output()?;
    if !check.status.success() {
        let _ = fs::remove_file(&temporary);
        return Err("Проверка новой версии не пройдена. Текущий бинарный файл не изменён.".into());
    }

    fs::rename(&temporary, &executable)?;
    println!("Обновление завершено.");
    Ok(())
}

pub fn uninstall() -> AppResult<()> {
    let executable = std::env::current_exe()?;
    let subscriptions_path = Path::new(SUBSCRIPTIONS_FILE);
    let config_path = Path::new(CONFIG_FILE);

    let delete_subscriptions = loop {
        match read_input("Удалить файл sub.json с подписками?\n1. Да\n2. Нет\nВыберите пункт: ")?.as_str() {
            "1" => break true,
            "2" => break false,
            _ => println!("Введите 1 или 2."),
        }
    };

    println!("Будут удалены:");
    println!("- исполняемый файл {}", executable.display());
    println!("- файл {}", config_path.display());
    if delete_subscriptions {
        println!("- файл {}", subscriptions_path.display());
    } else {
        println!("- файл {} будет сохранён", subscriptions_path.display());
    }

    match read_input("Подтвердить удаление?\n1. Да\n2. Нет\nВыберите пункт: ")?.as_str() {
        "1" => {}
        "2" => {
            println!("Удаление отменено.");
            return Ok(());
        }
        _ => {
            println!("Неизвестный пункт. Удаление отменено.");
            return Ok(());
        }
    }

    if delete_subscriptions {
        remove_if_exists(subscriptions_path)?;
    }
    remove_if_exists(config_path)?;

    if Path::new(CONFIG_DIR).exists() {
        if fs::read_dir(CONFIG_DIR)?.next().is_none() {
            fs::remove_dir(CONFIG_DIR)?;
        }
    }

    remove_if_exists(&executable)?;
    println!("xkeen-sub удалён.");
    Ok(())
}

fn detect_asset() -> AppResult<&'static str> {
    let entware_arch = Command::new("opkg")
        .args(["print-architecture"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).lines()
            .filter_map(|line| line.split_whitespace().nth(1))
            .filter(|arch| *arch != "all")
            .last()
            .unwrap_or("")
            .to_owned())
        .unwrap_or_default();

    let uname_arch = Command::new("uname").arg("-m").output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .unwrap_or_default();

    match entware_arch.as_str() {
        "aarch64-3.10" | "aarch64-k3.10" | "aarch64" => Ok("xkeen-sub-aarch64"),
        "mipsel-3.4" | "mipsel" => Ok("xkeen-sub-mipsel"),
        "mips-3.4" | "mips" => Ok("xkeen-sub-mips"),
        _ => match uname_arch.as_str() {
            "aarch64" => Ok("xkeen-sub-aarch64"),
            "mipsel" => Ok("xkeen-sub-mipsel"),
            "mips" => Ok("xkeen-sub-mips"),
            _ => Err(format!(
                "Архитектура не поддерживается (Entware: {}, uname: {}).",
                if entware_arch.is_empty() { "не определена" } else { &entware_arch },
                if uname_arch.is_empty() { "не определена" } else { &uname_arch }
            ).into()),
        },
    }
}

fn download(url: &str, destination: &Path) -> AppResult<()> {
    let status = if command_exists("curl") {
        Command::new("curl").args(["-fL", "--silent", "--show-error", url, "-o"])
            .arg(destination).status()?
    } else if command_exists("wget") {
        Command::new("wget").arg("-O").arg(destination).arg(url).status()?
    } else {
        return Err("Для обновления нужен curl или wget.".into());
    };

    if !status.success() {
        return Err("Не удалось скачать бинарный файл из последнего GitHub Release.".into());
    }
    Ok(())
}

fn command_exists(command: &str) -> bool {
    Command::new("sh").args(["-c", &format!("command -v {command} >/dev/null 2>&1")])
        .status().map(|status| status.success()).unwrap_or(false)
}

fn temporary_path(executable: &Path, operation: &str) -> PathBuf {
    executable.with_file_name(format!(".xkeen-sub.{operation}.{}", std::process::id()))
}

fn remove_if_exists(path: &Path) -> AppResult<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn read_input(prompt: &str) -> AppResult<String> {
    print!("{prompt}");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    Ok(input.trim().to_owned())
}
