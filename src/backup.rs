use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::error::Error;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::process::Command;

use crate::terminal::{error, info, info_prefix, menu_item, menu_item_prefix, read_input, success, title, warning, ProgressBar};

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
    planned_files: usize,
    saved_files: usize,
    outcome_status: String,
    outcome_summary: String,
    allow_prune: bool,
    subscriptions: Vec<String>,
    updated_subscriptions: Vec<String>,
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
        let pid = std::process::id();
        let header = format!(
            "============================================================\n\
             XKEEN-SUB — ЖУРНАЛ ОБНОВЛЕНИЯ\n\
             ============================================================\n\
             ID резервной записи: {id}\n\
             Дата и время: {date} {now}\n\
             PID процесса: {pid}\n\
             Причина создания: {reason}\n\
             Подписки: определяются\n\
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

        let mut session = Self {
            id, path, log_path, finished: false,
            planned_files: 0, saved_files: 0,
            outcome_status: "ВЫПОЛНЯЕТСЯ".to_owned(),
            outcome_summary: "Обновление ещё выполняется.".to_owned(),
            allow_prune: false,
            subscriptions: Vec::new(),
            updated_subscriptions: Vec::new(),
        };
        session.event("UPDATE", "Создана резервная запись. Начато выполнение update.")?;
        session.event("UPDATE", &format!("Причина создания: {reason}"))?;
        Ok(session)
    }

    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    pub(crate) fn set_subscriptions(&mut self, subscriptions: Vec<String>) {
        self.subscriptions = subscriptions;
    }

    pub(crate) fn set_updated_subscriptions(&mut self, subscriptions: Vec<String>) {
        self.updated_subscriptions = subscriptions;
    }

    pub(crate) fn is_running(&self) -> bool {
        self.outcome_status == "ВЫПОЛНЯЕТСЯ"
    }

    pub(crate) fn event(&mut self, level: &str, message: &str) -> AppResult<()> {
        let line = match (current_date(), current_time()) {
            (Ok(date), Ok(time)) => format!("[{date} {time}] {level}: {message}\n"),
            _ => format!("[дата и время неизвестны] {level}: {message}\n"),
        };
        let mut file = OpenOptions::new().append(true).open(&self.log_path)
            .map_err(|e| format!("Не удалось открыть журнал {}: {e}", self.log_path.display()))?;
        file.write_all(line.as_bytes())?;
        file.flush()?;
        file.sync_all()?;
        Ok(())
    }

    pub(crate) fn set_plan_count(&mut self, count: usize) {
        self.planned_files = count;
    }

    pub(crate) fn backup_files(&mut self, sources: &[BackupSource]) -> AppResult<usize> {
        self.backup_files_inner(sources, None)
    }

    pub(crate) fn backup_files_with_progress(&mut self, sources: &[BackupSource], progress: &mut ProgressBar) -> AppResult<usize> {
        self.backup_files_inner(sources, Some(progress))
    }

    fn backup_files_inner(&mut self, sources: &[BackupSource], mut progress: Option<&mut ProgressBar>) -> AppResult<usize> {
        if sources.is_empty() {
            self.event("BACKUP", "Нет существующих файлов, требующих резервного копирования.")?;
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
            let source_metadata = fs::symlink_metadata(&source.path)
                .map_err(|e| format!("Не удалось проверить исходный путь {}: {e}", source.path.display()))?;
            if source_metadata.file_type().is_symlink() {
                return Err(format!("Резервирование символических ссылок не поддерживается: {}", source.path.display()).into());
            }
            let original = source.path.canonicalize()
                .map_err(|e| format!("Не удалось определить исходный путь {}: {e}", source.path.display()))?;
            if !original.is_file() {
                return Err(format!("Резервируемый объект не является обычным файлом: {}", original.display()).into());
            }
            let name = original.file_name().and_then(|v| v.to_str())
                .ok_or_else(|| format!("Недопустимое имя файла: {}", original.display()))?;
            let duplicate = basename_counts.get(name).copied().unwrap_or(0) > 1 || name == "log.txt" || name == ".log.txt.tmp";
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
            )?;
            self.append_file_mapping(&original, &stored_relative)?;
            self.append_file_metadata(&original, source_metadata.mode() & 0o7777, source_metadata.uid(), source_metadata.gid(), &original_bytes)?;
            saved += 1;
            self.saved_files = saved;
            if let Some(progress) = progress.as_deref_mut() {
                progress.advance("Создание резервной копии");
            }
        }

        self.saved_files = saved;
        self.event("BACKUP", &format!("Побайтовая проверка успешно пройдена для {saved} файлов."))?;
        Ok(saved)
    }

    pub(crate) fn verify_original(&self, source: &Path) -> AppResult<bool> {
        let target = canonical_target_path(source)?;
        let mappings = read_file_mappings(&self.log_path)?;
        let Some(stored) = mappings.iter().find(|(original, _)| original == &target).map(|(_, stored)| stored) else {
            return Ok(false);
        };
        let backup_file = self.path.join(stored);
        let backup_metadata = match fs::symlink_metadata(&backup_file) {
            Ok(metadata) if !metadata.file_type().is_symlink() && metadata.is_file() => metadata,
            _ => return Ok(false),
        };
        let _ = backup_metadata;
        let expected = match fs::read(backup_file) {
            Ok(bytes) => bytes,
            Err(_) => return Ok(false),
        };
        let source_metadata = match fs::symlink_metadata(&target) {
            Ok(metadata) if !metadata.file_type().is_symlink() && metadata.is_file() => metadata,
            _ => return Ok(false),
        };
        let actual = match fs::read(&target) {
            Ok(bytes) => bytes,
            Err(_) => return Ok(false),
        };
        if actual != expected { return Ok(false); }
        let Some(expected_metadata) = read_file_metadata(&self.log_path, &target)? else {
            return Ok(false);
        };
        if file_checksum(&expected) != expected_metadata.checksum { return Ok(false); }
        Ok(source_metadata.mode() & 0o7777 == expected_metadata.mode
            && source_metadata.uid() == expected_metadata.uid
            && source_metadata.gid() == expected_metadata.gid)
    }

    pub(crate) fn restore_original(&mut self, source: &Path) -> AppResult<()> {
        let target = canonical_target_path(source)?;
        let mappings = read_file_mappings(&self.log_path)?;
        let stored = mappings.iter().find(|(original, _)| original == &target)
            .map(|(_, stored)| stored.clone())
            .ok_or_else(|| format!("В резервной копии нет исходного файла: {}", target.display()))?;
        let backup_file = self.path.join(&stored);
        let backup_metadata = fs::symlink_metadata(&backup_file)
            .map_err(|error| format!("Не удалось проверить резервный файл {}: {error}", backup_file.display()))?;
        if backup_metadata.file_type().is_symlink() || !backup_metadata.is_file() {
            return Err(format!("Резервный объект не является обычным файлом: {}", backup_file.display()).into());
        }
        let backup_bytes = fs::read(&backup_file)
            .map_err(|error| format!("Не удалось прочитать резервный файл {}: {error}", backup_file.display()))?;
        let metadata = read_file_metadata(&self.log_path, &target)?
            .ok_or_else(|| format!("В журнале отсутствуют метаданные исходного файла: {}", target.display()))?;
        if file_checksum(&backup_bytes) != metadata.checksum {
            return Err(format!("Контрольная сумма резервного файла не совпадает: {}", backup_file.display()).into());
        }
        restore_file_atomically(&target, &backup_bytes, metadata)?;
        self.event("ROLLBACK", &format!("Исходный файл восстановлен и проверен: {}", target.display())).ok();
        Ok(())
    }

    fn append_file_metadata(&mut self, original: &Path, mode: u32, uid: u32, gid: u32, bytes: &[u8]) -> AppResult<()> {
        let path = serde_json::to_string(&original.display().to_string())?;
        let checksum = file_checksum(bytes);
        let line = format!("Метаданные: {path} -> {mode:o} {uid} {gid} {checksum:016x}\n");
        let mut file = OpenOptions::new().append(true).open(&self.log_path)?;
        file.write_all(line.as_bytes())?;
        file.flush()?;
        file.sync_all()?;
        Ok(())
    }

    fn append_file_mapping(&mut self, original: &Path, stored: &Path) -> AppResult<()> {
        // This human-readable mapping is also the restore index; no separate metadata file is stored.
        let original_value = serde_json::to_string(&original.display().to_string())?;
        let stored_value = serde_json::to_string(&stored.to_string_lossy().to_string())?;
        let line = format!("Файл: {original_value} -> {stored_value}\n");
        let mut file = OpenOptions::new().append(true).open(&self.log_path)?;
        file.write_all(line.as_bytes())?;
        file.flush()?;
        file.sync_all()?;
        Ok(())
    }

    pub(crate) fn set_outcome(&mut self, status: &str, summary: &str, allow_prune: bool) {
        self.outcome_status = status.to_owned();
        self.outcome_summary = summary.to_owned();
        self.allow_prune = allow_prune;
    }

    pub(crate) fn finish(&mut self) -> AppResult<()> {
        if self.finished {
            return Ok(());
        }
        self.event("UPDATE", &self.outcome_summary.clone())?;
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
             PID процесса: {}\n\
             Обрабатываемые подписки: {}\n\
             Подписки с изменениями: {}\n\
             Итоговый статус: {}\n\
             Планируется изменить файлов: {}\n\
             Сохранено файлов: {}\n\
             Результат: {}\n\
             ------------------------------------------------------------\n",
            self.id, current_date()?, now, std::process::id(), if self.subscriptions.is_empty() { "—".to_owned() } else { self.subscriptions.join(", ") }, if self.updated_subscriptions.is_empty() { "—".to_owned() } else { self.updated_subscriptions.join(", ") }, self.outcome_status, self.planned_files, self.saved_files, self.outcome_summary
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

        if self.allow_prune {
            match load_settings().and_then(|settings| prune_records(&settings, &self.id)) {
                Ok(removed) if removed > 0 => self.event("RETENTION", &format!("Автоматически удалено старых резервных записей: {removed}."))?,
                Ok(_) => {}
                Err(error) => self.event("RETENTION", &format!("Автоматическая очистка не выполнена; резервные записи сохранены: {error}"))?,
            }
        }
        Ok(())
    }
}

pub fn run() -> AppResult<()> {
    run_menu().map(|_| ())
}

pub fn run_command(args: &[String]) -> AppResult<()> {
    match args {
        [] => run_menu().map(|_| ()),
        [action] if action == "help" => { print_help(); Ok(()) },
        [action] if action == "list" => list_interactive().map(|_| ()),
        [action] if action == "full" => create_full_backup(),
        [action, id] if action == "list" => show_record_menu(id).map(|_| ()),
        [action, value] if action == "days" => {
            let days = u32::try_from(parse_number(value, MIN_RETENTION_DAYS as u64, "срок хранения")?)
                .map_err(|_| "Срок хранения слишком велик.")?;
            let mut settings = load_settings()?;
            settings.retention_days = days;
            save_settings(&settings)?;
            success(&format!("Срок хранения резервных копий — {days} дней."));
            Ok(())
        }
        [action, value] if action == "limit" => {
            let limit = usize::try_from(parse_number(value, MIN_DAILY_LIMIT as u64, "дневной лимит")?)
                .map_err(|_| "Дневной лимит слишком велик.")?;
            let mut settings = load_settings()?;
            settings.daily_limit = limit;
            save_settings(&settings)?;
            success(&format!("Дневной лимит резервных копий — {limit} записей."));
            Ok(())
        }
        [action, value] if action == "mode" => {
            let mut settings = load_settings()?;
            settings.mode = parse_mode(value)?;
            save_settings(&settings)?;
            success(&format!("Режим хранения изменён: {}.", mode_label(&settings.mode)));
            Ok(())
        }
        _ => {
            print_help();
            Ok(())
        }
    }
}

fn run_menu() -> AppResult<bool> {
    loop {
        let settings = load_settings()?;
        println!();
        title("XKEEN-SUB — НАСТРОЙКИ РЕЗЕРВИРОВАНИЯ");
        crate::terminal::separator("────────────────────────────────────────");
        info("Защита включена: перед изменением файлов сохраняется резервная копия.");
        menu_item("1", &format!("Срок хранения: {} дней (минимум)", settings.retention_days));
        menu_item("2", &format!("Лимит записей за день: {} (минимум)", settings.daily_limit));
        menu_item("3", &format!("Режим хранения: {}", mode_label(&settings.mode)));
        menu_item("4", "Просмотреть резервные копии");
        menu_item("5", "Создать полный бэкап конфигураций");
        menu_item("6", "Назад");
        menu_item("0", "Выход");
        let choice = read_input("Выберите действие: ")?;
        match choice.as_str() {
            "1" => change_days()?,
            "2" => change_limit()?,
            "3" => change_mode()?,
            "4" => { if list_interactive()? { return Ok(true); } },
            "5" => create_full_backup()?,
            "6" => return Ok(false),
            "0" => return Ok(true),
            _ => error("Выберите пункт от 1 до 6 или 0 для выхода."),
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
                success(&format!("Готово! Резервные копии будут храниться {days} дней."));
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
                success(&format!("Готово! В день можно сохранять до {limit} резервных копий."));
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
                success("Автоматическая очистка включена: старые копии будут удаляться по заданным правилам.");
                return Ok(());
            }
            "2" => {
                let mut settings = load_settings()?;
                settings.mode = RetentionMode::Unlimited;
                save_settings(&settings)?;
                success("Резервные копии будут храниться, пока вы не удалите их вручную.");
                return Ok(());
            }
            "0" => return Ok(()),
            _ => error("Введите 1, 2 или 0."),
        }
    }
}

fn create_full_backup() -> AppResult<()> {
    title("СОЗДАНИЕ ПОЛНОГО БЭКАПА");
    info("Будут сохранены все обычные файлы конфигурации Xray и xkeen-sub.");
    info("Включая подписки, baseline, архив пользовательских подключений и настройки резервирования.");
    if !confirm("Создать полную резервную копию текущего состояния?")? {
        warning("Создание полного бэкапа отменено.");
        return Ok(());
    }

    let mut sources = Vec::new();
    collect_backup_files(Path::new("/opt/etc/xray/configs"), "Конфигурация Xray", &mut sources)?;
    collect_backup_files(Path::new("/opt/etc/xkeen-sub"), "Конфигурация xkeen-sub", &mut sources)?;
    sources.retain(|source| !source.path.starts_with(Path::new(BACKUP_DIR)));

    if sources.is_empty() {
        return Err("Не найдено файлов конфигурации для резервирования.".into());
    }
    sources.sort_by(|a, b| a.path.cmp(&b.path));

    let mut backup = BackupSession::start("Полный бэкап конфигураций Xray и xkeen-sub")?;
    backup.set_plan_count(sources.len());
    let saved = match backup.backup_files(&sources) {
        Ok(saved) if saved == sources.len() => saved,
        Ok(saved) => {
            let message = format!("Сохранено {saved} из {} файлов; полный бэкап не подтверждён.", sources.len());
            backup.set_outcome("ОШИБКА", &message, false);
            backup.finish()?;
            return Err(message.into());
        }
        Err(error) => {
            backup.set_outcome("ОШИБКА", &format!("Полный бэкап не завершён: {error}"), false);
            backup.finish()?;
            return Err(error);
        }
    };
    backup.set_outcome("ПОЛНЫЙ БЭКАП СОЗДАН", &format!("Сохранено и проверено {saved} файлов конфигурации Xray и xkeen-sub."), true);
    backup.finish()?;
    success(&format!("Полный бэкап создан. Сохранено и проверено файлов: {saved}. ID: {}", backup.id()));
    Ok(())
}

fn collect_backup_files(root: &Path, category: &str, output: &mut Vec<BackupSource>) -> AppResult<()> {
    let metadata = match fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(format!("Каталог конфигурации не найден: {}", root.display()).into());
        }
        Err(error) => return Err(format!("Не удалось проверить каталог {}: {error}", root.display()).into()),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(format!("Каталог конфигурации не является обычным каталогом: {}", root.display()).into());
    }

    let mut entries = fs::read_dir(root)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| format!("Не удалось проверить {}: {error}", path.display()))?;
        if metadata.file_type().is_symlink() {
            return Err(format!("Полный бэкап прерван: обнаружена символическая ссылка {}.", path.display()).into());
        }
        if metadata.is_dir() {
            if path == Path::new(BACKUP_DIR) {
                continue;
            }
            collect_backup_files(&path, category, output)?;
        } else if metadata.is_file() {
            output.push(BackupSource { path, category: category.to_owned() });
        } else {
            return Err(format!("Полный бэкап прерван: неподдерживаемый объект {}.", path.display()).into());
        }
    }
    Ok(())
}

fn list_interactive() -> AppResult<bool> {
    loop {
        let records = load_records()?;
        title("XKEEN-SUB — РЕЗЕРВНЫЕ КОПИИ");
        if records.is_empty() {
            info("Резервных копий пока нет. Они появятся после первого обновления.");
            return Ok(false);
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
            menu_item_prefix(&(index + 1).to_string(), &format!("backup{}", record.number), &format!("{time} · {status} · файлов сохранено: {saved}"));
        }
        println!();
        info(&format!("Всего резервных копий: {}", records.len()));
        info(&format!("Период хранения: {}", if settings.mode == RetentionMode::Unlimited {
            "без ограничений".to_owned()
        } else {
            format!("{} календарных дней", settings.retention_days)
        }));
        info(&format!("Дневной лимит: {}", settings.daily_limit));

        let input = read_input("Введите номер записи или 0 — назад: ")?;
        if input == "0" {
            return Ok(false);
        }
        match input.parse::<usize>() {
            Ok(number) if (1..=records.len()).contains(&number) => {
                let id = records[number - 1].id.clone();
                if show_record_menu(&id)? { return Ok(true); }
            }
            _ => error(&format!("Введите номер от 1 до {} или 0.", records.len())),
        }
    }
}

fn show_record_menu(id: &str) -> AppResult<bool> {
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
        menu_item("0", "Выход");
        match read_input("Выберите действие: ")?.as_str() {
            "1" => { if show_log(&record.path.join("log.txt"))? { return Ok(true); } },
            "2" => { if show_files(&record)? { return Ok(true); } },
            "3" => restore_record(&record)?,
            "4" => {
                if is_active_record(&record) {
                    warning("Нельзя удалить резервную запись, пока она используется работающим update.");
                    continue;
                }
                if confirm(&format!("Удалить резервную запись {}? Это действие нельзя отменить.", record.id))? {
                    fs::remove_dir_all(&record.path)?;
                    success("Резервная копия удалена.");
                    return Ok(false);
                }
            }
            "5" => return Ok(false),
            "0" => return Ok(true),
            _ => error("Выберите пункт от 1 до 5 или 0 для выхода."),
        }
    }
}

fn show_log(path: &Path) -> AppResult<bool> {
    title("ПОДРОБНЫЙ ЖУРНАЛ");
    let content = fs::read_to_string(path)?;
    println!("{content}");
    Ok(read_input("Enter — назад, 0 — выход: ")?.trim() == "0")
}

fn show_files(record: &BackupRecord) -> AppResult<bool> {
    title("ФАЙЛЫ В РЕЗЕРВНОЙ КОПИИ");
    let log_path = record.path.join("log.txt");
    let files = read_file_mappings(&log_path)?;
    let created = read_created_paths(&log_path)?;
    if files.is_empty() && created.is_empty() {
        info("В этой резервной записи нет файловых операций.");
    } else {
        for (source, stored) in files {
            info_prefix(&source.display().to_string(), &format!("сохранён в копии как {}.", stored.display()));
        }
        for path in created {
            info_prefix(&path.display().to_string(), "будет удалён при восстановлении — файл создан обновлением.");
        }
    }
    Ok(read_input("Enter — назад, 0 — выход: ")?.trim() == "0")
}

fn restore_record(record: &BackupRecord) -> AppResult<()> {
    if is_active_record(record) {
        warning("Нельзя восстанавливать резервную запись, пока она используется работающим update.");
        return Ok(());
    }
    let status = read_summary(&record.path.join("log.txt")).get("status").cloned().unwrap_or_default();
    if status != "РЕЗЕРВНАЯ КОПИЯ СОЗДАНА" && status != "ПОЛНЫЙ БЭКАП СОЗДАН" && status != "УСПЕШНО" {
        warning("Эта запись не содержит подтверждённую полную резервную копию. Восстановление запрещено.");
        return Ok(());
    }
    let log_path = record.path.join("log.txt");
    let mappings = read_file_mappings(&log_path)?;
    let created_paths = read_created_paths(&log_path)?;
    if mappings.is_empty() && created_paths.is_empty() {
        warning("В резервной записи нет файловых операций для восстановления.");
        return Ok(());
    }

    let mut targets = BTreeSet::new();
    for (source, _) in &mappings {
        targets.insert(source.clone());
    }
    for path in &created_paths {
        targets.insert(path.clone());
    }
    if !confirm(&format!(
        "Восстановить резервную запись {}? Будут возвращены {} сохранённых файлов и удалены {} файлов, созданных обновлением.",
        record.id, mappings.len(), created_paths.len()
    ))? {
        warning("Восстановление отменено.");
        return Ok(());
    }

    let mut existing = Vec::new();
    let mut missing_before = Vec::new();
    for source in &targets {
        if source.exists() {
            existing.push(BackupSource { path: source.clone(), category: "Перед восстановлением".to_owned() });
        } else {
            missing_before.push(source.clone());
        }
    }

    let mut safety = BackupSession::start("Резервная копия перед ручным восстановлением")?;
    safety.set_plan_count(existing.len());
    let safety_saved = match safety.backup_files(&existing) {
        Ok(count) => count,
        Err(error) => {
            safety.set_outcome("ОШИБКА", &format!("Не удалось создать защитную копию перед восстановлением: {error}"), false);
            let _ = safety.finish();
            return Err(error);
        }
    };
    safety.set_outcome("УСПЕШНО", "Создана защитная копия текущего состояния перед восстановлением.", false);
    safety.finish()?;

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
            let bytes = fs::read(&saved)
                .map_err(|e| format!("Не удалось прочитать резервный файл {}: {e}", saved.display()))?;
            let metadata = read_file_metadata(&record.path.join("log.txt"), source)?
                .ok_or_else(|| format!("В журнале отсутствуют метаданные файла: {}", source.display()))?;
            if file_checksum(&bytes) != metadata.checksum {
                return Err(format!("Контрольная сумма резервного файла не совпадает: {}", saved.display()).into());
            }
            restore_file_atomically(source, &bytes, metadata)?;
        }

        for path in &created_paths {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(format!("Не удалось удалить созданный обновлением файл {}: {error}", path.display()).into()),
            }
            if path.exists() {
                return Err(format!("Файл, созданный обновлением, остался на месте: {}", path.display()).into());
            }
        }
        Ok(())
    })();

    if let Err(error) = restore_result {
        for source in &missing_before {
            let _ = fs::remove_file(source);
        }
        if safety_saved > 0 {
            if let Ok(safety_record) = find_record(safety.id()) {
                if let Ok(safety_mappings) = read_file_mappings(&safety_record.path.join("log.txt")) {
                    for (source, stored) in safety_mappings {
                        let saved = safety_record.path.join(stored);
                        if saved.is_file() {
                            if fs::copy(&saved, &source).is_ok() {
                                if fs::read(&saved).ok() != fs::read(&source).ok() {
                                    let _ = safety.event("ROLLBACK", &format!("Побайтовая проверка отката не пройдена: {}", source.display()));
                                } else {
                                    let _ = safety.event("ROLLBACK", &format!("Исходное состояние восстановлено: {}", source.display()));
                                }
                            } else {
                                let _ = safety.event("ROLLBACK", &format!("Не удалось восстановить исходное состояние: {}", source.display()));
                            }
                        }
                    }
                }
            }
        }
        safety.event("RESTORE", &format!("Восстановление не удалось; выполнена попытка отката: {error}"))?;
        return Err(error);
    }

    safety.event("RESTORE", &format!("Резервная запись {} восстановлена; все файлы прошли побайтовую проверку.", record.id))?;
    match load_settings().and_then(|settings| prune_records(&settings, safety.id())) {
        Ok(removed) if removed > 0 => safety.event("RETENTION", &format!("После восстановления удалено старых записей: {removed}."))?,
        Ok(_) => {}
        Err(error) => {
            safety.event("RETENTION", &format!("Не удалось выполнить очистку после восстановления: {error}"))?;
            warning("Восстановление завершено, но автоматическая очистка резервных копий не выполнена.");
        }
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
    command_help("backup full", "Создать полный бэкап конфигураций Xray и xkeen-sub");
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
            result.insert("time".to_owned(), value.split_whitespace().nth(1).unwrap_or(value).to_owned());
        } else if let Some(value) = line.strip_prefix("Дата и время: ") {
            result.entry("time".to_owned()).or_insert_with(|| value.split_whitespace().nth(1).unwrap_or(value).to_owned());
        } else if let Some(value) = line.strip_prefix("PID процесса: ") {
            result.insert("pid".to_owned(), value.to_owned());
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
        let source: String = serde_json::from_str(source)
            .map_err(|e| format!("Некорректный исходный путь в журнале: {e}"))?;
        let stored: String = serde_json::from_str(stored)
            .map_err(|e| format!("Некорректный путь резервной копии в журнале: {e}"))?;
        let source = PathBuf::from(source);
        let stored = PathBuf::from(stored);
        if !source.is_absolute() || !is_allowed_restore_path(&source) {
            return Err("В журнале обнаружен недопустимый исходный путь для восстановления.".into());
        }
        validate_relative_path(&stored)?;
        mappings.push((source, stored));
    }
    Ok(mappings)
}

fn read_created_paths(log_path: &Path) -> AppResult<Vec<PathBuf>> {
    let content = fs::read_to_string(log_path)
        .map_err(|e| format!("Не удалось прочитать журнал резервной записи: {e}"))?;
    let mut paths = BTreeSet::new();
    for line in content.lines() {
        let Some((_, operation)) = line.split_once("] PLAN: Создание: ") else { continue; };
        let path_value = operation.split(" | category=").next().unwrap_or(operation);
        let path_text: String = serde_json::from_str(path_value)
            .map_err(|e| format!("Некорректный путь созданного файла в журнале: {e}"))?;
        let path = PathBuf::from(path_text);
        if !path.is_absolute() || !is_allowed_restore_path(&path) {
            return Err("В журнале обнаружен недопустимый путь созданного файла.".into());
        }
        paths.insert(path);
    }
    Ok(paths.into_iter().collect())
}

fn canonical_target_path(source: &Path) -> AppResult<PathBuf> {
    let parent = source.parent().ok_or("Не удалось определить каталог исходного файла.")?;
    let canonical_parent = parent.canonicalize()
        .map_err(|error| format!("Не удалось определить каталог {}: {error}", parent.display()))?;
    let name = source.file_name().ok_or("У исходного файла отсутствует имя.")?;
    Ok(canonical_parent.join(name))
}

#[derive(Clone, Copy, Debug)]
struct FileMetadata {
    mode: u32,
    uid: u32,
    gid: u32,
    checksum: u64,
}

fn file_checksum(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn read_file_metadata(log_path: &Path, source: &Path) -> AppResult<Option<FileMetadata>> {
    let content = fs::read_to_string(log_path)?;
    for line in content.lines() {
        let Some(value) = line.strip_prefix("Метаданные: ") else { continue; };
        let Some((path, metadata)) = value.split_once(" -> ") else { continue; };
        let path: String = serde_json::from_str(path)
            .map_err(|error| format!("Некорректный путь метаданных в журнале: {error}"))?;
        if Path::new(&path) != source { continue; }
        let mut fields = metadata.split_whitespace();
        let mode = u32::from_str_radix(fields.next().ok_or("В журнале отсутствует режим файла.")?, 8)?;
        let uid = fields.next().ok_or("В журнале отсутствует UID файла.")?.parse::<u32>()?;
        let gid = fields.next().ok_or("В журнале отсутствует GID файла.")?.parse::<u32>()?;
        let checksum = u64::from_str_radix(fields.next().ok_or("В журнале отсутствует контрольная сумма файла.")?, 16)?;
        if fields.next().is_some() { return Err("Некорректная запись метаданных файла.".into()); }
        return Ok(Some(FileMetadata { mode, uid, gid, checksum }));
    }
    Ok(None)
}

fn restore_file_atomically(path: &Path, content: &[u8], metadata: FileMetadata) -> AppResult<()> {
    let parent = path.parent().ok_or("Не удалось определить каталог восстанавливаемого файла.")?;
    fs::create_dir_all(parent)?;
    if let Ok(current) = fs::symlink_metadata(path) {
        if current.file_type().is_symlink() || !current.is_file() {
            return Err(format!("Нельзя восстановить файл поверх небезопасного объекта: {}", path.display()).into());
        }
    }
    let counter = crate::temp_counter::next();
    let name = path.file_name().and_then(|value| value.to_str()).ok_or("Недопустимое имя файла восстановления.")?;
    let temporary = parent.join(format!(".{name}.xkeen-sub-restore-{}-{counter}.tmp", std::process::id()));
    let mut temporary_created = false;
    let result = (|| -> AppResult<()> {
        let mut file = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temporary)?;
        temporary_created = true;
        file.write_all(content)?;
        file.sync_all()?;
        drop(file);
        let owner = Command::new("chown").arg(format!("{}:{}", metadata.uid, metadata.gid)).arg("--").arg(&temporary).output()
            .map_err(|error| format!("Не удалось запустить chown для {}: {error}", temporary.display()))?;
        if !owner.status.success() {
            return Err(format!("Не удалось восстановить владельца {}: {}", temporary.display(), String::from_utf8_lossy(&owner.stderr)).into());
        }
        fs::set_permissions(&temporary, fs::Permissions::from_mode(metadata.mode))?;
        if fs::read(&temporary)? != content {
            return Err(format!("Побайтовая проверка временного файла не пройдена: {}", temporary.display()).into());
        }
        fs::rename(&temporary, path)?;
        if fs::read(path)? != content {
            return Err(format!("Побайтовая проверка восстановленного файла не пройдена: {}", path.display()).into());
        }
        let restored = fs::metadata(path)?;
        if restored.mode() & 0o7777 != metadata.mode || restored.uid() != metadata.uid || restored.gid() != metadata.gid {
            return Err(format!("Проверка владельца или прав восстановленного файла не пройдена: {}", path.display()).into());
        }
        Ok(())
    })();
    if let Err(original_error) = &result {
        if temporary_created {
            if let Err(cleanup) = fs::remove_file(&temporary) {
                if cleanup.kind() != std::io::ErrorKind::NotFound {
                    return Err(format!("{original_error}; не удалось удалить временный файл {}: {cleanup}", temporary.display()).into());
                }
            }
        }
    }
    result
}

fn is_allowed_restore_path(path: &Path) -> bool {
    let archive = Path::new("/opt/etc/xkeen-sub/old-user-outbound.json");
    let subscriptions = Path::new("/opt/etc/xkeen-sub/sub.json");
    let xray_root = Path::new("/opt/etc/xray/configs");
    let baseline_root = Path::new("/opt/etc/xkeen-sub/baseline");
    if path == archive || path == subscriptions {
        return true;
    }
    let xkeen_root = Path::new("/opt/etc/xkeen-sub");
    let backup_root = Path::new(BACKUP_DIR);
    let has_parent = path.components().any(|component| matches!(component, Component::ParentDir));
    !has_parent && (path.starts_with(xray_root) || (path.starts_with(xkeen_root) && !path.starts_with(backup_root)))
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
            if record.id != active_id && !is_active_or_incomplete_record(&record) {
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
            if record.id != active_id && !is_active_or_incomplete_record(&record) && !keep.contains(&record.id) {
                fs::remove_dir_all(&record.path)?;
                removed += 1;
            }
        }
    }
    Ok(removed)
}

fn is_active_or_incomplete_record(record: &BackupRecord) -> bool {
    let summary = read_summary(&record.path.join("log.txt"));
    if !summary.contains_key("status") {
        return true;
    }
    is_active_record(record)
}

fn is_active_record(record: &BackupRecord) -> bool {
    let summary = read_summary(&record.path.join("log.txt"));
    let Some(pid) = summary.get("pid").and_then(|value| value.parse::<u32>().ok()) else {
        return summary.get("status").map(String::as_str) == Some("ВЫПОЛНЯЕТСЯ");
    };
    if Path::new("/proc").is_dir() {
        Path::new(&format!("/proc/{pid}")).exists()
    } else {
        summary.get("status").map(String::as_str) == Some("ВЫПОЛНЯЕТСЯ")
    }
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
    use super::{days_from_civil, file_checksum, is_allowed_restore_path, parse_mode, parse_record_id, validate_relative_path, BackupSettings, RetentionMode};
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
    fn default_backup_settings_keep_backups_enabled_with_safe_minimums() {
        let settings = BackupSettings::default();
        assert_eq!(settings.retention_days, 3);
        assert_eq!(settings.daily_limit, 10);
        assert_eq!(settings.mode, RetentionMode::Automatic);
    }

    #[test]
    fn restore_paths_are_limited_to_update_managed_locations() {
        assert!(is_allowed_restore_path(Path::new("/opt/etc/xray/configs/04_outbounds.json")));
        assert!(is_allowed_restore_path(Path::new("/opt/etc/xkeen-sub/baseline/sub1--Germany.json")));
        assert!(is_allowed_restore_path(Path::new("/opt/etc/xkeen-sub/old-user-outbound.json")));
        assert!(is_allowed_restore_path(Path::new("/opt/etc/xkeen-sub/sub.json")));
        assert!(is_allowed_restore_path(Path::new("/opt/etc/xkeen-sub/backup-settings.json")));
        assert!(!is_allowed_restore_path(Path::new("/opt/etc/xkeen-sub/backup/14-11-2026--backup1/file.json")));
        assert!(!is_allowed_restore_path(Path::new("/etc/passwd")));
        assert!(!is_allowed_restore_path(Path::new("/opt/etc/xray/configs/../passwd")));
    }

    #[test]
    fn backup_checksum_detects_corrupted_contents() {
        assert_eq!(file_checksum(b"original"), file_checksum(b"original"));
        assert_ne!(file_checksum(b"original"), file_checksum(b"corrupted"));
    }

    #[test]
    fn parses_only_supported_retention_modes() {
        assert_eq!(parse_mode("auto").unwrap(), RetentionMode::Automatic);
        assert_eq!(parse_mode("unlimited").unwrap(), RetentionMode::Unlimited);
        assert!(parse_mode("off").is_err());
    }

    #[test]
    fn rejects_paths_that_escape_backup_directory() {
        assert!(validate_relative_path(Path::new("../outside")).is_err());
        assert!(validate_relative_path(Path::new("/absolute")).is_err());
        assert!(validate_relative_path(Path::new("opt/etc/file.json")).is_ok());
    }
}
