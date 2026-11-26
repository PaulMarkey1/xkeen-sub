use std::cmp::Ordering;
use std::error::Error;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::system::command_exists;
use crate::terminal::{error, info, info_prefix, menu_item, menu_item_prefix, read_input, success, title};

type AppResult<T> = Result<T, Box<dyn Error>>;

const CONFIG_DIR: &str = "/opt/etc/xkeen-sub";
const SUBSCRIPTIONS_FILE: &str = "/opt/etc/xkeen-sub/sub.json";
const CONFIG_FILE: &str = "/opt/etc/xkeen-sub/config.json";
const REPO: &str = "PaulMarkey1/xkeen-sub";

pub fn update() -> AppResult<()> {
    let executable = std::env::current_exe()?;
    let asset = detect_asset()?;
    let current_version = env!("XKEEN_SUB_VERSION");
    let release_file = temporary_path(&executable, "latest-release");

    title("ОБНОВЛЕНИЕ ПРОГРАММЫ");
    info(&format!("Текущая версия: xkeen-sub {current_version}"));
    info("Проверяем, доступна ли новая версия...");

    let release_url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    if let Err(error) = download(&release_url, &release_file) {
        let _ = fs::remove_file(&release_file);
        return Err(error);
    }

    let release_text = match fs::read_to_string(&release_file) {
        Ok(text) => text,
        Err(error) => {
            let _ = fs::remove_file(&release_file);
            return Err(error.into());
        }
    };
    let _ = fs::remove_file(&release_file);

    let release: serde_json::Value = serde_json::from_str(&release_text)
        .map_err(|_| "GitHub вернул некорректные данные последнего релиза.")?;
    let latest_tag = release.get("tag_name")
        .and_then(serde_json::Value::as_str)
        .ok_or("В ответе GitHub отсутствует тег последнего релиза.")?;

    match compare_release_versions(latest_tag, current_version) {
        Some(Ordering::Equal) => {
            info(&format!("Уже установлена последняя версия: {latest_tag}."));
            return Ok(());
        }
        Some(Ordering::Less) => {
            info(&format!(
                "Установленная версия новее последнего релиза ({latest_tag}). Откат не выполняем."
            ));
            return Ok(());
        }
        Some(Ordering::Greater) => {}
        None => {
            return Err(format!(
                "Не удалось безопасно сравнить установленную версию ({current_version}) и релиз ({latest_tag}). Обновление отменено."
            ).into());
        }
    }

    let download_url = release.get("assets")
        .and_then(serde_json::Value::as_array)
        .and_then(|assets| assets.iter().find(|item| {
            item.get("name").and_then(serde_json::Value::as_str) == Some(asset)
        }))
        .and_then(|item| item.get("browser_download_url"))
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| format!(
            "В релизе {latest_tag} нет бинарного файла {asset} для архитектуры этого роутера."
        ))?
        .to_owned();

    let temporary = temporary_path(&executable, "update");
    info(&format!("Загружаем новую версию {latest_tag}..."));
    if let Err(error) = download(&download_url, &temporary) {
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

    let check = match Command::new(&temporary).arg("--help").output() {
        Ok(output) => output,
        Err(error) => {
            let _ = fs::remove_file(&temporary);
            return Err(format!("Не удалось запустить загруженную версию: {error}").into());
        }
    };
    if !check.status.success() {
        let _ = fs::remove_file(&temporary);
        return Err("Проверка новой версии не пройдена. Текущий бинарный файл не изменён.".into());
    }

    let downloaded_version = match binary_version(&temporary) {
        Some(version) => version,
        None => {
            let _ = fs::remove_file(&temporary);
            return Err("Не удалось определить версию загруженного бинарного файла. Текущая программа не изменена.".into());
        }
    };
    if !same_release_version(&downloaded_version, latest_tag) {
        let _ = fs::remove_file(&temporary);
        return Err(format!(
            "Версия бинарного файла ({downloaded_version}) не совпадает с тегом релиза ({latest_tag}). Текущая программа не изменена."
        ).into());
    }

    if let Err(error) = fs::rename(&temporary, &executable) {
        let _ = fs::remove_file(&temporary);
        return Err(error.into());
    }
    success(&format!("Обновление завершено: {downloaded_version}."));
    Ok(())
}

pub fn reinstall() -> AppResult<()> {
    let executable = std::env::current_exe()?;
    let asset = detect_asset()?;
    let releases_file = temporary_path(&executable, "releases");

    title("ПЕРЕУСТАНОВКА ПРОГРАММЫ");
    info(&format!("Текущая версия: xkeen-sub {}", env!("XKEEN_SUB_VERSION")));
    info("Получаем список доступных версий...");
    let releases_url = format!("https://api.github.com/repos/{REPO}/releases?per_page=5");
    let fetch_result = download(&releases_url, &releases_file);
    if let Err(error) = fetch_result {
        let _ = fs::remove_file(&releases_file);
        return Err(error);
    }

    let releases_text = fs::read_to_string(&releases_file)?;
    let _ = fs::remove_file(&releases_file);
    let releases: serde_json::Value = serde_json::from_str(&releases_text)
        .map_err(|_| "GitHub вернул некорректный список релизов.")?;
    let releases = releases.as_array()
        .ok_or("GitHub вернул неожиданный формат списка релизов.")?;

    let mut choices = Vec::new();
    for release in releases {
        let tag = match release.get("tag_name").and_then(serde_json::Value::as_str) {
            Some(value) => value,
            None => continue,
        };
        let published_at = release.get("published_at")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("дата неизвестна");
        let download_url = release.get("assets")
            .and_then(serde_json::Value::as_array)
            .and_then(|assets| assets.iter().find(|item| {
                item.get("name").and_then(serde_json::Value::as_str) == Some(asset)
            }))
            .and_then(|item| item.get("browser_download_url"))
            .and_then(serde_json::Value::as_str);

        if let Some(url) = download_url {
            choices.push((tag.to_owned(), published_at.to_owned(), url.to_owned()));
        }
    }

    if choices.is_empty() {
        return Err(format!(
            "В последних пяти релизах нет бинарного файла {asset} для архитектуры этого роутера."
        ).into());
    }

    println!();
    title("ДОСТУПНЫЕ ВЕРСИИ ДЛЯ ЭТОЙ АРХИТЕКТУРЫ");
    for (index, (tag, date, _)) in choices.iter().enumerate() {
        menu_item_prefix(&(index + 1).to_string(), tag, &format!("выпущена {date}"));
    }
    menu_item("0", "Отмена");

    let selected = loop {
        let input = read_input("Выберите версию для установки: ")?;
        if input == "0" {
            info("Переустановка отменена. Установленная версия осталась прежней.");
            return Ok(());
        }
        match input.parse::<usize>() {
            Ok(number) if (1..=choices.len()).contains(&number) => break number - 1,
            _ => error(&format!("Введите номер от 1 до {} или 0, чтобы отменить установку.", choices.len())),
        }
    };

    let (tag, _, url) = &choices[selected];
    let temporary = temporary_path(&executable, "reinstall");
    info(&format!("Вы выбрали версию {tag}."));
    info(&format!("Скачиваем версию {tag}..."));
    let download_result = download(url, &temporary);
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

    let check = match Command::new(&temporary).arg("--help").output() {
        Ok(output) => output,
        Err(error) => {
            let _ = fs::remove_file(&temporary);
            return Err(format!("Не удалось запустить загруженную версию {tag}: {error}").into());
        }
    };
    if !check.status.success() {
        let _ = fs::remove_file(&temporary);
        return Err(format!(
            "Проверка {tag} не пройдена. Текущая версия не изменена."
        ).into());
    }

    match binary_version(&temporary) {
        Some(installed_version) if installed_version != format!("xkeen-sub {tag}") => {
            let _ = fs::remove_file(&temporary);
            return Err(format!(
                "Версия загруженного бинарного файла не совпадает с выбранным релизом: ожидалась xkeen-sub {tag}, получена {installed_version}. Текущая версия не изменена."
            ).into());
        }
        Some(installed_version) => {
            info(&format!("Версия загруженной программы проверена: {installed_version}."));
        }
        None => {
            info(&format!("Номер версии в файле не определился. Выбранный релиз: {tag}."));
        }
    }

    if let Err(error) = fs::rename(&temporary, &executable) {
        let _ = fs::remove_file(&temporary);
        return Err(error.into());
    }
    success(&format!("Готово! Установлена версия {tag}."));
    info("Список ваших подписок сохранён без изменений.");
    Ok(())
}

pub fn uninstall() -> AppResult<()> {
    let executable = std::env::current_exe()?;
    let subscriptions_path = Path::new(SUBSCRIPTIONS_FILE);
    let config_path = Path::new(CONFIG_FILE);

    let delete_subscriptions = loop {
        title("УДАЛЕНИЕ ПРОГРАММЫ");
        info("Удалить файл sub.json с подписками?");
        menu_item("1", "Да");
        menu_item("2", "Нет");
        match read_input("Выберите пункт: ")?.as_str() {
            "1" => break true,
            "2" => break false,
            _ => error("Введите 1 или 2."),
        }
    };

    println!();
    title("ЧТО БУДЕТ УДАЛЕНО");
    info_prefix(&executable.display().to_string(), "файл программы будет удалён");
    info_prefix(&config_path.display().to_string(), "остаточный файл настроек будет удалён");
    if delete_subscriptions {
        info_prefix(&subscriptions_path.display().to_string(), "файл подписок тоже будет удалён");
    } else {
        info_prefix(&subscriptions_path.display().to_string(), "файл подписок останется на месте");
    }

    println!();
    title("ПОДТВЕРЖДЕНИЕ УДАЛЕНИЯ");
    menu_item("1", "Да");
    menu_item("2", "Нет");
    match read_input("Выберите пункт: ")?.as_str() {
        "1" => {}
        "2" => {
            info("Удаление отменено. Файлы программы и подписок сохранены.");
            return Ok(());
        }
        _ => {
            error("Неизвестный выбор. Удаление отменено, файлы не затронуты.");
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
    success("Программа xkeen-sub удалена.");
    Ok(())
}

fn same_release_version(binary_version: &str, release_tag: &str) -> bool {
    normalize_version(binary_version) == normalize_version(release_tag)
}

fn normalize_version(version: &str) -> &str {
    let version = version.trim();
    let version = version.strip_prefix("xkeen-sub ").unwrap_or(version);
    version.strip_prefix('v').unwrap_or(version)
}

fn compare_release_versions(latest: &str, installed: &str) -> Option<Ordering> {
    let latest = parse_release_version(latest)?;
    let installed = parse_release_version(installed)?;

    match latest.0.cmp(&installed.0) {
        Ordering::Equal => match (latest.1, installed.1) {
            (false, false) => Some(Ordering::Equal),
            (false, true) => Some(Ordering::Greater),
            (true, false) => Some(Ordering::Less),
            // Unknown ordering between different prerelease labels: fail safe.
            (true, true) => {
                if normalize_version(latest.2) == normalize_version(installed.2) {
                    Some(Ordering::Equal)
                } else {
                    None
                }
            }
        },
        ordering => Some(ordering),
    }
}

fn parse_release_version(version: &str) -> Option<((u64, u64, u64), bool, &str)> {
    let normalized = normalize_version(version).split('+').next()?;
    let (core, is_prerelease) = match normalized.split_once('-') {
        Some((core, _)) => (core, true),
        None => (normalized, false),
    };
    let mut components = core.split('.');
    let major = components.next()?.parse().ok()?;
    let minor = components.next()?.parse().ok()?;
    let patch = components.next()?.parse().ok()?;
    if components.next().is_some() {
        return None;
    }
    Some(((major, minor, patch), is_prerelease, normalized))
}

fn binary_version(path: &Path) -> Option<String> {
    let output = Command::new(path).arg("--version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let version = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if version.starts_with("xkeen-sub ") {
        Some(version)
    } else {
        None
    }
}

fn detect_asset() -> AppResult<&'static str> {
    let entware_arch = Command::new("opkg")
        .args(["print-architecture"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).lines()
            .filter_map(|line| line.split_whitespace().nth(1))
            .filter(|arch| *arch != "all" && *arch != "noarch")
            .last()
            .unwrap_or("")
            .to_owned())
        .unwrap_or_default();

    let uname_arch = Command::new("uname").arg("-m").output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .unwrap_or_default();

    if !entware_arch.is_empty() {
        return asset_for_entware_arch(&entware_arch).ok_or_else(|| {
            format!(
                "Архитектура Entware не поддерживается: {} (uname: {}).",
                entware_arch,
                if uname_arch.is_empty() { "не определена" } else { &uname_arch }
            ).into()
        });
    }

    // uname reports "mips" for both MIPS endiannesses. Never guess between
    // mips and mipsel when opkg cannot provide the Entware ABI.
    match uname_arch.as_str() {
        "aarch64" => Ok("xkeen-sub-aarch64"),
        "mipsel" => Ok("xkeen-sub-mipsel"),
        "mips" => Err(
            "Невозможно определить endian MIPS без архитектуры Entware (uname: mips).".into()
        ),
        _ => Err(format!(
            "Архитектура не поддерживается (Entware: не определена, uname: {}).",
            if uname_arch.is_empty() { "не определена" } else { &uname_arch }
        ).into()),
    }
}

fn asset_for_entware_arch(arch: &str) -> Option<&'static str> {
    if arch == "aarch64" || arch.starts_with("aarch64-") {
        Some("xkeen-sub-aarch64")
    } else if arch == "mipsel" || arch.starts_with("mipsel-") {
        Some("xkeen-sub-mipsel")
    } else if arch == "mips" || arch.starts_with("mips-") {
        Some("xkeen-sub-mips")
    } else {
        None
    }
}

fn download(url: &str, destination: &Path) -> AppResult<()> {
    let status = if command_exists("curl") {
        Command::new("curl")
            .args(["-fL", "--silent", "--show-error",
                "-H", "Accept: application/vnd.github+json",
                "-H", "User-Agent: xkeen-sub",
                url, "-o"])
            .arg(destination).status()?
    } else if command_exists("wget") {
        Command::new("wget")
            .arg("--header=Accept: application/vnd.github+json")
            .arg("--header=User-Agent: xkeen-sub")
            .arg("-O").arg(destination).arg(url).status()?
    } else {
        return Err("Для обновления нужен curl или wget.".into());
    };

    if !status.success() {
        return Err("Не удалось скачать бинарный файл из последнего GitHub Release.".into());
    }
    Ok(())
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



#[cfg(test)]
mod architecture_tests {
    use super::asset_for_entware_arch;

    #[test]
    fn release_version_comparison_handles_v_prefix_and_patch_updates() {
        assert_eq!(super::compare_release_versions("v0.3.18", "0.3.17"), Some(std::cmp::Ordering::Greater));
        assert_eq!(super::compare_release_versions("v0.3.17", "0.3.17"), Some(std::cmp::Ordering::Equal));
        assert_eq!(super::compare_release_versions("0.3.16", "v0.3.17"), Some(std::cmp::Ordering::Less));
    }

    #[test]
    fn stable_release_is_newer_than_same_prerelease() {
        assert_eq!(super::compare_release_versions("v0.3.17", "v0.3.17-rc.1"), Some(std::cmp::Ordering::Greater));
    }

    #[test]
    fn refuses_ambiguous_prerelease_order() {
        assert_eq!(super::compare_release_versions("v0.3.17-rc.2", "v0.3.17-rc.1"), None);
    }

    #[test]
    fn maps_entware_architecture_suffixes() {
        assert_eq!(asset_for_entware_arch("mipsel-3.4_kn"), Some("xkeen-sub-mipsel"));
        assert_eq!(asset_for_entware_arch("mipsel-3.4"), Some("xkeen-sub-mipsel"));
        assert_eq!(asset_for_entware_arch("mips-3.4_kn"), Some("xkeen-sub-mips"));
        assert_eq!(asset_for_entware_arch("mips-3.4"), Some("xkeen-sub-mips"));
        assert_eq!(asset_for_entware_arch("aarch64-3.10"), Some("xkeen-sub-aarch64"));
        assert_eq!(asset_for_entware_arch("aarch64-k3.10"), Some("xkeen-sub-aarch64"));
        assert_eq!(asset_for_entware_arch("x86_64"), None);
    }
}
