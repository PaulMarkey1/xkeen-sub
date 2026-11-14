use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::error::Error;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use crate::terminal::{error, info, menu_item, read_input, success, title, warning};

type AppResult<T> = Result<T, Box<dyn Error>>;

pub(crate) const BACKUP_DIR: &str = "/opt/etc/xkeen-sub/backup";
const SETTINGS_FILE: &str = "/opt/etc/xkeen-sub/backup-settings.json";
const MIN_RETENTION_DAYS: u32 = 3;
const MIN_DAILY_LIMIT: usize = 10;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub(crate) struct BackupSettings {
    pub(crate) retention_days: u32,
    pub(crate) daily_limit: usize,
    pub(crate) mode: RetentionMode,
}

impl Default for BackupSettings {
    fn default() -> Self {
        Self {
            retention_days: 3,
            daily_limit: 10,
            mode: RetentionMode::Automatic,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RetentionMode {
    Automatic,
    Unlimited,
}

impl Default for RetentionMode {
    fn default() -> Self {
        Self::Automatic
    }
}

#[derive(Clone, Debug)]
pub(crate) struct BackupSource {
    pub(crate) path: PathBuf,
    pub(crate) category: String,
}

#[derive(Clone, Debug)]
struct BackupRecord {
    id: String,
    path: PathBuf,
    date: String,
    number: usize,
}

pub(crate) struct BackupSession {
    id: String,
    path: PathBuf,
    log_path: PathBuf,
    finished: bool,
}

impl BackupSession {
    pub(crate) fn start(reason: &str) -> AppResult<Self> {
        let root = Path::new(BACKUP_DIR);
        fs::create_dir_all(root)
            .map_err(|e| format!("Не удалось создать каталог резервных копий {}: {e}", root.display()))?;

        let date = current_date()?;
        let mut number = next_number(root, &date)?;
        let (id, path) = loop {
            let id = format!("{date}--backup{number}");
            let path = root.join(&id);
            match fs::create_dir(&path) {
                Ok(()) => break (id, path),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => number += 1,
                Err(e) => return Err(format!("Не удалось создать резервную запись {id}: {e}").into()),
            }
        };

        let log_path = path.join("log.txt");
        let mut log = OpenOptions::new().write(true).create_new(true).open(&log_path)
            .map_err(|e| format!("Не удалось создать {}: {e}", log_path.display()))?;
        let now = current_time()?;
        let header = format!(
            "============================================================\n\
             XKEEN-SUB — ЖУРНАЛ ОБНОВЛЕНИЯ\n\
             ============================================================\n\
             ID резервной записи: {id}\n\
             Дата и время: {date} {now}\n\
             Причина создания: {reason}\n\
             Итоговый статус: ВЫПОЛНЯЕТСЯ\n\
             Планируется изменить файлов: 0\n\
             Сохранено файлов: 0\n\
             Результат: Обновление ещё выполняется.\n\
             ------------------------------------------------------------\n\
             ПОДРОБНЫЙ ЖУРНАЛ\n\
             ------------------------------------------------------------\n"
        );
        log.write_all(header.as_bytes())?;
        log.sync_all()?;

        let mut session = Self { id, path, log_path, finished: false };
        session.event("UPDATE", "Создана резервная запись. Начато выполнение update.");
        Ok(session)
    }

    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn event(&mut self, level: &str, message: &str) {
        let line = match current_time() {
            Ok(time) => format!("[{time}] {level}: {message}\n"),
            Err(_) => format!("[время неизвестно] {level}: {message}\n"),
        };
        if let Ok(mut file) = OpenOptions::new().append(true).open(&self.log_path) {
            let _ = file.write_all(line.as_bytes());
            let _ = file.flush();
            let _ = file.sync_all();
        }
    }

    pub(crate) fn backup_files(&mut self, sources: &[BackupSource]) -> AppResult<usize> {
        if sources.is_empty() {
            self.event("BACKUP", "Нет существующих файлов, требующих резервного копирования.");
            return Ok(0);
        }

        let mut basename_counts: HashMap<String, usize> = HashMap::new();
        for source in sources {
            let name = source.path.file_name().and_then(|v| v.to_str())
                .ok_or_else(|| format!("Не удалось определить имя файла {}", source.path.display()))?;
            *basename_counts.entry(name.to_owned()).or_default() += 1;
        }

        let mut saved = 0usize;
        for source in sources {
            let original = source.path.canonicalize()
                .map_err(|e| format!("Не удалось определить исходный путь {}: {e}", source.path.display()))?;
            if !original.is_file() {
                return Err(format!("Резервируемый объект не является обычным файлом: {}", original.display()).into());
            }
            let name = original.file_name().and_then(|v| v.to_str())
                .ok_or_else(|| format!("Недопустимое имя файла: {}", original.display()))?;
            let duplicate = basename_counts.get(name).copied().unwrap_or(0) > 1 || name == "log.txt";
            let stored_relative = if duplicate {
                original.strip_prefix("/").map_err(|_| "Исходный путь должен быть абсолютным.")?.to_path_buf()
            } else {
                PathBuf::from(name)
            };
            validate_relative_path(&stored_relative)?;
            let destination = self.path.join(&stored_relative);
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }

            let original_bytes = fs::read(&original)
                .map_err(|e| format!("Не удалось прочитать {}: {e}", original.display()))?;
            fs::copy(&original, &destination)
                .map_err(|e| format!("Не удалось скопировать {}: {e}", original.display()))?;
            let copied_bytes = fs::read(&destination)
                .map_err(|e| format!("Не удалось проверить копию {}: {e}", destination.display()))?;
            if original_bytes != copied_bytes {
                let _ = fs::remove_file(&destination);
                return Err(format!("Побайтовая проверка резервной копии не пройдена: {}", original.display()).into());
            }

            let stored = stored_relative.to_string_lossy();
            self.event(
                "BACKUP",
                &format!("Сохранён файл: {} -> {} ({} байт; категория: {}).", original.display(), stored, original_bytes.len(), source.category),
            );
            self.append_file_mapping(&original, &stored_relative)?;
            saved += 1;
        }

        self.event("BACKUP", &format!("Побайтовая проверка успешно пройдена для {saved} файлов."));
        Ok(saved)
    }

    fn append_file_mapping(&mut self, original: &Path, stored: &Path) -> AppResult<()> {
        // This human-readable mapping is also the restore index; no separate metadata file is stored.
        let line = format!("Файл: {} -> {}\n", original.display(), stored.display());
        let mut file = OpenOptions::new().append(true).open(&self.log_path)?;
        file.write_all(line.as_bytes())?;
        file.flush()?;
        file.sync_all()?;
        Ok(())
    }

    pub(crate) fn finish(
        &mut self,
        status: &str,
        summary: &str,
        planned_files: usize,
        saved_files: usize,
        allow_prune: bool,
    ) -> AppResult<()> {
        if self.finished {
            return Ok(());
        }
        self.event("UPDATE", summary);
        let existing = fs::read_to_string(&self.log_path)?;
        let marker = "ПОДРОБНЫЙ ЖУРНАЛ\n";
        let detail_start = existing.find(marker)
            .ok_or("В журнале отсутствует раздел подробной хронологии.")?;
        let details = &existing[detail_start..];
        let now = current_time()?;
        let header = format!(
            "============================================================\n\
             XKEEN-SUB — ЖУРНАЛ ОБНОВЛЕНИЯ\n\
             ============================================================\n\
             ID резервной записи: {}\n\
             Дата и время завершения: {} {}\n\
             Итоговый статус: {status}\n\
             Планируется изменить файлов: {planned_files}\n\
             Сохранено файлов: {saved_files}\n\
             Результат: {summary}\n\
             ------------------------------------------------------------\n",
            self.id, current_date()?, now
        );
        let temporary = self.path.join(".log.txt.tmp");
        {
            let mut file = File::create(&temporary)?;
            file.write_all(header.as_bytes())?;
            file.write_all(details.as_bytes())?;
            file.sync_all()?;
        }
        fs::rename(&temporary, &self.log_path)?;
        self.finished = true;

        if allow_prune {
            match prune_records(&load_settings()?, &self.id) {
                Ok(removed) if removed > 0 => self.event("RETENTION", &format!("Автоматически удалено старых резервных записей: {removed}.")),
                Ok(_) => {}
                Err(error) => self.event("RETENTION", &format!("Не удалось выполнить очистку старых записей: {error}")),
            }
        }
        Ok(())
    }
}

pub fn run() -> AppResult<()> {
    run_menu()
}

pub fn run_command(args: &[String]) -> AppResult<()> {
    match args {
        [] => run_menu(),
        [action] if action == "list" => list_interactive(),
        [action, id] if action == "list" => show_record_menu(id),
        [action, value] if action == "days" => {
            let days = parse_number(value, MIN_RETENTION_DAYS, "срок хранения")? as u32;
            let mut settings = load_settings()?;
            settings.retention_days = days;
            save_settings(&settings)?;
            success(&format!("Срок хранения установлен: {days} календарных дней."));
            Ok(())
        }
        [action, value] if action == "limit" => {
            let limit = parse_number(value, MIN_DAILY_LIMIT as u64, "дневной лимит")? as usize;
            let mut settings = load_settings()?;
            settings.daily_limit = limit;
            save_settings(&settings)?;
            success(&format!("Дневной лимит установлен: {limit} записей."));
            Ok(())
        }
        [action, value] if action == "mode" => {
            let mut settings = load_settings()?;
            settings.mode = parse_mode(value)?;
            save_settings(&settings)?;
            success(&format!("Режим хранения: {}.", mode_label(&settings.mode)));
            Ok(())
        }
        _ => {
            print_help();
            Ok(())
        }
    }
}

fn run_menu() -> AppResult<()> {
    loop {
        let settings = load_settings()?;
        println!();
        title("XKEEN-SUB — НАСТРОЙКИ РЕЗЕРВИРОВАНИЯ");
        crate::terminal::separator("────────────────────────────────────────");
        info("Резервное копирование: ВКЛЮЧЕНО");
        menu_item("1", &format!("Срок хранения: {} дней (минимум)", settings.retention_days));
        menu_item("2", &format!("Лимит записей за день: {} (минимум)", settings.daily_limit));
        menu_item("3", &format!("Режим хранения: {}", mode_label(&settings.mode)));
        menu_item("4", "Просмотреть резервные копии");
        menu_item("5", "Назад");
        let choice = read_input("Введите соответствующую цифру для изменения параметра: ")?;
        match choice.as_str() {
            "1" => change_days()?,
            "2" => change_limit()?,
            "3" => change_mode()?,
            "4" => list_interactive()?,
            "5" => return Ok(()),
            _ => error("Неизвестный пункт меню. Введите цифру от 1 до 5."),
        }
    }
}

fn change_days() -> AppResult<()> {
    let current = load_settings()?.retention_days;
    title("СРОК ХРАНЕНИЯ");
    info(&format!("Текущее значение: {current} дней"));
    info(&format!("Минимальное значение: {MIN_RETENTION_DAYS} дня"));
    loop {
        let value = read_input("Введите новое количество дней (0 — отмена): ")?;
        if value == "0" { return Ok(()); }
        match value.parse::<u32>() {
            Ok(days) if days >= MIN_RETENTION_DAYS => {
                let mut settings = load_settings()?;
                settings.retention_days = days;
                save_settings(&settings)?;
                success(&format!("Срок хранения изменён: {days} дней."));
                return Ok(());
            }
            _ => error(&format!("Введите целое число не меньше {MIN_RETENTION_DAYS} или 0 для отмены.")),
        }
    }
}

fn change_limit() -> AppResult<()> {
    let current = load_settings()?.daily_limit;
    title("ЛИМИТ ЗАПИСЕЙ ЗА ДЕНЬ");
    info(&format!("Текущее значение: {current}"));
    info(&format!("Минимальное значение: {MIN_DAILY_LIMIT}"));
    loop {
        let value = read_input("Введите максимальное количество записей за день (0 — отмена): ")?;
        if value == "0" { return Ok(()); }
        match value.parse::<usize>() {
            Ok(limit) if limit >= MIN_DAILY_LIMIT => {
                let mut settings = load_settings()?;
                settings.daily_limit = limit;
                save_settings(&settings)?;
                success(&format!("Дневной лимит изменён: {limit} записей."));
                return Ok(());
            }
            _ => error(&format!("Введите целое число не меньше {MIN_DAILY_LIMIT} или 0 для отмены.")),
        }
    }
}

fn change_mode() -> AppResult<()> {
    let settings = load_settings()?;
    title("РЕЖИМ ХРАНЕНИЯ");
    info(&format!("Текущий режим: {}", mode_label(&settings.mode)));
    menu_item("1", "Автоматический");
    menu_item("2", "Бессрочный");
    loop {
        match read_input("Выберите режим (0 — отмена): ")?.as_str() {
            "1" => {
                let mut settings = load_settings()?;
                settings.mode = RetentionMode::Automatic;
                save_settings(&settings)?;
                success("Включено автоматическое удаление по установленным правилам.");
                return Ok(());
            }
            "2" => {
                let mut settings = load_settings()?;
                settings.mode = RetentionMode::Unlimited;
                save_settings(&settings)?;
                success("Бессрочное хранение включено. Автоматическое удаление отключено.");
                return Ok(());
            }
            "0" => return Ok(()),
            _ => error("Введите 1, 2 или 0."),
        }
    }
}

fn list_interactive() -> AppResult<()> {
    let records = load_records()?;
    title("XKEEN-SUB — РЕЗЕРВНЫЕ КОПИИ");
    if records.is_empty() {
        info("Резервные копии отсутствуют.");
        return Ok(());
    }

    let settings = load_settings()?;
    println!();
    let mut previous_date = String::new();
    for (index, record) in records.iter().enumerate() {
        if record.date != previous_date {
            println!();
            crate::terminal::separator(&record.date);
            previous_date = record.date.clone();
        }
        let summary = read_summary(&record.path.join("log.txt"));
        let time = summary.get("time").cloned().unwrap_or_else(|| "неизвестно".to_owned());
        let status = summary.get("status").cloned().unwrap_or_else(|| "неизвестно".to_owned());
        let saved = summary.get("saved").cloned().unwrap_or_else(|| "?".to_owned());
        menu_item(&(index + 1).to_string(), &format!("backup{} — {} — {} — сохранено файлов: {}", record.number, time, status, saved));
    }
    println!();
    info(&format!("Всего резервных копий: {}", records.len()));
    info(&format!("Период хранения: {} дней", if settings.mode == RetentionMode::Unlimited { "без ограничений".to_owned() } else { settings.retention_days.to_string() }));
    info(&format!("Дневной лимит: {}", settings.daily_limit));
    loop {
        let input = read_input("Введите номер записи для просмотра или 0 для выхода: ")?;
        if input == "0" { return Ok(()); }
        match input.parse::<usize>() {
            Ok(number) if (1..=records.len()).contains(&number) => return show_record_menu(&records[number - 1].id),
            _ => error(&format!("Введите номер от 1 до {} или 0.", records.len())),
        }
    }
}

fn show_record_menu(id: &str) -> AppResult<()> {
    let record = find_record(id)?;
    loop {
        let summary = read_summary(&record.path.join("log.txt"));
        println!();
        title("XKEEN-SUB — РЕЗЕРВНАЯ КОПИЯ");
        crate::terminal::separator("────────────────────────────────────────");
        info(&format!("ID: {}", record.id));
        info(&format!("Дата: {}", record.date));
        info(&format!("Время: {}", summary.get("time").map(String::as_str).unwrap_or("неизвестно")));
        info(&format!("Результат обновления: {}", summary.get("status").map(String::as_str).unwrap_or("неизвестно")));
        info(&format!("Сохранено файлов: {}", summary.get("saved").map(String::as_str).unwrap_or("?")));
        menu_item("1", "Просмотреть подробный журнал");
        menu_item("2", "Просмотреть список файлов");
        menu_item("3", "Восстановить резервную копию");
        menu_item("4", "Удалить резервную копию");
        menu_item("5", "Назад");
        match read_input("Введите соответствующую цифру: ")?.as_str() {
            "1" => show_log(&record.path.join("log.txt"))?,
            "2" => show_files(&record)?,
            "3" => restore_record(&record)?,
            "4" => {
                if confirm(&format!("Удалить резервную запись {}? Это действие нельзя отменить.", record.id))? {
                    fs::remove_dir_all(&record.path)?;
                    success("Резервная копия удалена.");
                    return Ok(());
                }
            }
            "5" => return Ok(()),
            _ => error("Неизвестный пункт. Введите цифру от 1 до 5."),
        }
    }
}

fn show_log(path: &Path) -> AppResult<()> {
    title("ПОДРОБНЫЙ ЖУРНАЛ");
    let content = fs::read_to_string(path)?;
    println!("{content}");
    let _ = read_input("Нажмите Enter для возврата...")?;
    Ok(())
}

fn show_files(record: &BackupRecord) -> AppResult<()> {
    title("ФАЙЛЫ В РЕЗЕРВНОЙ КОПИИ");
    let files = read_file_mappings(&record.path.join("log.txt"))?;
    if files.is_empty() {
        info("В этой резервной записи нет сохранённых исходных файлов.");
    } else {
        for (source, stored) in files {
            info(&format!("{}  →  {}", source.display(), stored.display()));
        }
    }
    let _ = read_input("Нажмите Enter для возврата...")?;
    Ok(())
}

fn restore_record(record: &BackupRecord) -> AppResult<()> {
    let mappings = read_file_mappings(&record.path.join("log.txt"))?;
    if mappings.is_empty() {
        warning("В резервной записи нет файлов для восстановления.");
        return Ok(());
    }
    if !confirm(&format!("Восстановить {} файлов из {}? Текущие файлы будут заменены.", mappings.len(), record.id))? {
        warning("Восстановление отменено.");
        return Ok(());
    }

    let mut existing = Vec::new();
    let mut missing = Vec::new();
    for (source, _) in &mappings {
        if source.exists() {
            existing.push(BackupSource { path: source.clone(), category: "Перед восстановлением".to_owned() });
        } else {
            missing.push(source.clone());
        }
    }

    let mut safety = BackupSession::start("Резервная копия перед ручным восстановлением")?;
    let safety_saved = match safety.backup_files(&existing) {
        Ok(count) => count,
        Err(error) => {
            let _ = safety.finish("ОШИБКА", &format!("Не удалось создать защитную копию перед восстановлением: {error}"), existing.len(), 0, false);
            return Err(error);
        }
    };
    safety.finish("УСПЕШНО", "Создана защитная копия текущего состояния перед восстановлением.", existing.len(), safety_saved, true)?;

    let mut created_now = Vec::new();
    let restore_result: AppResult<()> = (|| {
        for (source, stored) in &mappings {
            validate_relative_path(stored)?;
            let saved = record.path.join(stored);
            if !saved.is_file() {
                return Err(format!("Файл резервной копии отсутствует: {}", saved.display()).into());
            }
            if let Some(parent) = source.parent() {
                fs::create_dir_all(parent)?;
            }
            if !source.exists() {
                created_now.push(source.clone());
            }
            fs::copy(&saved, source)
                .map_err(|e| format!("Не удалось восстановить {}: {e}", source.display()))?;
            if fs::read(&saved)? != fs::read(source)? {
                return Err(format!("Проверка восстановленного файла не пройдена: {}", source.display()).into());
            }
        }
        Ok(())
    })();

    if let Err(error) = restore_result {
        for source in &created_now {
            let _ = fs::remove_file(source);
        }
        if safety_saved > 0 {
            if let Ok(safety_record) = find_record(safety.id()) {
                if let Ok(safety_mappings) = read_file_mappings(&safety_record.path.join("log.txt")) {
                    for (source, stored) in safety_mappings {
                        let saved = safety_record.path.join(stored);
                        if saved.is_file() {
                            let _ = fs::copy(saved, source);
                        }
                    }
                }
            }
        }
        safety.event("RESTORE", &format!("Восстановление не удалось; выполнена попытка отката: {error}"));
        return Err(error);
    }

    success("Резервная копия восстановлена. Побайтовая проверка пройдена.");
    Ok(())
}

fn confirm(question: &str) -> AppResult<bool> {
    title("ПОДТВЕРЖДЕНИЕ");
    info(question);
    menu_item("1", "Да");
    menu_item("2", "Нет");
    loop {
        match read_input("Выберите пункт: ")?.as_str() {
            "1" => return Ok(true),
            "2" => return Ok(false),
            _ => error("Введите 1 или 2."),
        }
    }
}

fn load_settings() -> AppResult<BackupSettings> {
    match fs::read_to_string(SETTINGS_FILE) {
        Ok(text) => {
            let settings: BackupSettings = serde_json::from_str(&text)
                .map_err(|e| format!("Файл настроек резервирования повреждён: {e}"))?;
            if settings.retention_days < MIN_RETENTION_DAYS || settings.daily_limit < MIN_DAILY_LIMIT {
                return Err("Настройки резервирования содержат значение ниже допустимого минимума.".into());
            }
            Ok(settings)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(BackupSettings::default()),
        Err(error) => Err(error.into()),
    }
}

fn save_settings(settings: &BackupSettings) -> AppResult<()> {
    fs::create_dir_all(Path::new(SETTINGS_FILE).parent().ok_or("Не удалось определить каталог настроек.")?)?;
    let content = serde_json::to_vec_pretty(settings)?;
    let temporary = format!("{SETTINGS_FILE}.tmp");
    fs::write(&temporary, content)?;
    fs::rename(temporary, SETTINGS_FILE)?;
    Ok(())
}

fn parse_number(value: &str, minimum: u64, label: &str) -> AppResult<u64> {
    let number = value.parse::<u64>().map_err(|_| format!("Некорректное значение {label}: требуется целое число."))?;
    if number < minimum {
        return Err(format!("Значение {label} не может быть меньше {minimum}.").into());
    }
    Ok(number)
}

fn parse_mode(value: &str) -> AppResult<RetentionMode> {
    match value {
        "auto" => Ok(RetentionMode::Automatic),
        "unlimited" => Ok(RetentionMode::Unlimited),
        _ => Err("Режим должен быть auto или unlimited.".into()),
    }
}

fn mode_label(mode: &RetentionMode) -> &'static str {
    match mode {
        RetentionMode::Automatic => "Автоматический",
        RetentionMode::Unlimited => "Бессрочный",
    }
}

fn print_help() {
    title("XKEEN-SUB — УПРАВЛЕНИЕ РЕЗЕРВНЫМИ КОПИЯМИ");
    command_help("backup", "Открыть меню резервирования");
    command_help("backup days N", "Установить срок хранения в календарных днях (минимум 3)");
    command_help("backup limit N", "Установить дневной лимит записей (минимум 10)");
    command_help("backup mode auto|unlimited", "Выбрать автоматическое или бессрочное хранение");
    command_help("backup list", "Просмотреть резервные копии");
    command_help("backup list ID", "Открыть выбранную резервную запись");
}

fn command_help(command: &str, description: &str) {
    crate::terminal::command_item(command, description);
}

fn load_records() -> AppResult<Vec<BackupRecord>> {
    let root = Path::new(BACKUP_DIR);
    if !root.exists() { return Ok(Vec::new()); }
    let mut records = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() { continue; }
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Some((date, number)) = parse_record_id(&name) {
            records.push(BackupRecord { id: name, path: entry.path(), date, number });
        }
    }
    records.sort_by(|a, b| {
        let a_date = date_from_id(&a.id).unwrap_or(i64::MIN);
        let b_date = date_from_id(&b.id).unwrap_or(i64::MIN);
        a_date.cmp(&b_date).then(a.number.cmp(&b.number))
    });
    records.reverse();
    Ok(records)
}

fn find_record(id: &str) -> AppResult<BackupRecord> {
    let record = load_records()?.into_iter().find(|item| item.id == id)
        .ok_or_else(|| format!("Резервная запись {id} не найдена."))?;
    Ok(record)
}

fn parse_record_id(value: &str) -> Option<(String, usize)> {
    let (date, number) = value.split_once("--backup")?;
    let (day, month_year) = date.split_once('-')?;
    let (month, year) = month_year.split_once('-')?;
    if day.len() != 2 || month.len() != 2 || year.len() != 4
        || !day.chars().all(|c| c.is_ascii_digit())
        || !month.chars().all(|c| c.is_ascii_digit())
        || !year.chars().all(|c| c.is_ascii_digit())
    { return None; }
    let number = number.parse::<usize>().ok()?;
    if number == 0 { return None; }
    Some((date.to_owned(), number))
}

fn next_number(root: &Path, date: &str) -> AppResult<usize> {
    let mut max = 0usize;
    if !root.exists() { return Ok(1); }
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if let Some((record_date, number)) = parse_record_id(&entry.file_name().to_string_lossy()) {
            if record_date == date { max = max.max(number); }
        }
    }
    Ok(max + 1)
}

fn read_summary(path: &Path) -> HashMap<String, String> {
    let mut result = HashMap::new();
    let Ok(content) = fs::read_to_string(path) else { return result; };
    for line in content.lines() {
        if let Some(value) = line.strip_prefix("Дата и время завершения: ") {
            result.insert("time".to_owned(), value.to_owned());
        } else if let Some(value) = line.strip_prefix("Дата и время: ") {
            result.entry("time".to_owned()).or_insert_with(|| value.to_owned());
        } else if let Some(value) = line.strip_prefix("Итоговый статус: ") {
            result.insert("status".to_owned(), value.to_owned());
        } else if let Some(value) = line.strip_prefix("Сохранено файлов: ") {
            result.insert("saved".to_owned(), value.to_owned());
        }
    }
    result
}

fn read_file_mappings(log_path: &Path) -> AppResult<Vec<(PathBuf, PathBuf)>> {
    let content = fs::read_to_string(log_path)
        .map_err(|e| format!("Не удалось прочитать журнал резервной записи: {e}"))?;
    let mut mappings = Vec::new();
    for line in content.lines() {
        let Some(value) = line.strip_prefix("Файл: ") else { continue; };
        let Some((source, stored)) = value.split_once(" -> ") else { continue; };
        let source = PathBuf::from(source);
        let stored = PathBuf::from(stored);
        if !source.is_absolute() {
            return Err("В журнале обнаружен некорректный исходный путь.".into());
        }
        validate_relative_path(&stored)?;
        mappings.push((source, stored));
    }
    Ok(mappings)
}

fn validate_relative_path(path: &Path) -> AppResult<()> {
    if path.as_os_str().is_empty() || path.is_absolute()
        || path.components().any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(format!("Недопустимый относительный путь: {}", path.display()).into());
    }
    Ok(())
}

fn prune_records(settings: &BackupSettings, active_id: &str) -> AppResult<usize> {
    if settings.mode == RetentionMode::Unlimited { return Ok(0); }
    let records = load_records()?;
    let today = current_date()?;
    let today_ordinal = date_from_id(&format!("{today}--backup1"))
        .ok_or("Не удалось определить текущую календарную дату.")?;
    let mut removed = 0usize;
    let mut by_date: BTreeMap<String, Vec<BackupRecord>> = BTreeMap::new();

    for record in records {
        let ordinal = date_from_id(&record.id).unwrap_or(i64::MIN);
        if today_ordinal - ordinal >= settings.retention_days as i64 {
            if record.id != active_id {
                fs::remove_dir_all(&record.path)?;
                removed += 1;
            }
        } else {
            by_date.entry(record.date.clone()).or_default().push(record);
        }
    }

    for (_, mut day_records) in by_date {
        day_records.sort_by_key(|record| record.number);
        if day_records.len() <= settings.daily_limit { continue; }
        let first_id = day_records.iter().find(|record| record.number == 1).map(|record| record.id.clone());
        let mut keep = HashSet::new();
        if let Some(id) = first_id { keep.insert(id); }
        for record in day_records.iter().rev() {
            if keep.len() >= settings.daily_limit { break; }
            keep.insert(record.id.clone());
        }
        for record in day_records {
            if record.id != active_id && !keep.contains(&record.id) {
                fs::remove_dir_all(&record.path)?;
                removed += 1;
            }
        }
    }
    Ok(removed)
}

fn current_date() -> AppResult<String> {
    run_date("%d-%m-%Y")
}

fn current_time() -> AppResult<String> {
    run_date("%H:%M:%S")
}

fn run_date(format: &str) -> AppResult<String> {
    let output = Command::new("date").arg(format!("+{format}")).output()?;
    if !output.status.success() {
        return Err("Не удалось получить локальные дату и время системными средствами.".into());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn date_from_id(id: &str) -> Option<i64> {
    let (date, _) = parse_record_id(id)?;
    let mut parts = date.split('-');
    let day = parts.next()?.parse::<i64>().ok()?;
    let month = parts.next()?.parse::<i64>().ok()?;
    let year = parts.next()?.parse::<i64>().ok()?;
    if !(1..=31).contains(&day) || !(1..=12).contains(&month) || !(1970..=9999).contains(&year) {
        return None;
    }
    Some(days_from_civil(year, month, day))
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let adjusted_month = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * adjusted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146097 + day_of_era - 719468
}

#[cfg(test)]
mod tests {
    use super::{days_from_civil, parse_record_id, validate_relative_path};
    use std::path::Path;

    #[test]
    fn parses_daily_backup_ids() {
        assert_eq!(parse_record_id("14-11-2026--backup3"), Some(("14-11-2026".to_owned(), 3)));
        assert_eq!(parse_record_id("14-11-2026--backup0"), None);
        assert_eq!(parse_record_id("../backup1"), None);
    }

    #[test]
    fn computes_calendar_day_ordinals() {
        assert_eq!(days_from_civil(2026, 11, 14) - days_from_civil(2026, 11, 13), 1);
        assert_eq!(days_from_civil(2026, 1, 1) - days_from_civil(2025, 12, 31), 1);
    }

    #[test]
    fn rejects_paths_that_escape_backup_directory() {
        assert!(validate_relative_path(Path::new("../outside")).is_err());
        assert!(validate_relative_path(Path::new("/absolute")).is_err());
        assert!(validate_relative_path(Path::new("opt/etc/file.json")).is_ok());
    }
}
