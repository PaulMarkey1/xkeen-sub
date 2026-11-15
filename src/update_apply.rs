use crate::backup::BackupSession;
use std::error::Error;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

type AppResult<T> = Result<T, Box<dyn Error>>;

const STAGING_ROOT: &str = "/tmp/xkeen-sub-update";
const CONFIG_ROOT: &str = "/opt/etc/xray/configs";
const BASELINE_ROOT: &str = "/opt/etc/xkeen-sub/baseline";
const ARCHIVE_PATH: &str = "/opt/etc/xkeen-sub/old-user-outbound.json";
const MAX_FILE_ATTEMPTS: usize = 5;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(45);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PlannedOperationKind {
    Create,
    Replace,
    Delete,
}

pub(crate) struct PlannedFileOperation {
    pub(crate) path: PathBuf,
    pub(crate) category: &'static str,
    pub(crate) kind: PlannedOperationKind,
    pub(crate) content: Option<Vec<u8>>,
}

struct StagingDirectory {
    root: PathBuf,
}

impl StagingDirectory {
    fn create() -> AppResult<Self> {
        let root = PathBuf::from(STAGING_ROOT);
        fs::create_dir(&root).map_err(|error| {
            format!("Не удалось создать staging-каталог {} (он уже может существовать): {error}", root.display())
        })?;
        if let Err(error) = fs::set_permissions(&root, fs::Permissions::from_mode(0o700)) {
            let _ = fs::remove_dir_all(&root);
            return Err(format!("Не удалось ограничить права staging-каталога {}: {error}", root.display()).into());
        }
        for name in ["configs", "archive", "baseline"] {
            if let Err(error) = fs::create_dir(root.join(name)) {
                let _ = fs::remove_dir_all(&root);
                return Err(format!("Не удалось создать staging-подкаталог {name}: {error}").into());
            }
        }
        Ok(Self { root })
    }

    fn file_for(&self, operation: &PlannedFileOperation) -> AppResult<Option<PathBuf>> {
        if operation.kind == PlannedOperationKind::Delete {
            return Ok(None);
        }
        let folder = match operation.category {
            "Конфигурация Xray" => "configs",
            "Архив пользовательских подключений" => "archive",
            "Baseline" => "baseline",
            other => return Err(format!("Неизвестная категория операции: {other}").into()),
        };
        let name = operation.path.file_name().ok_or("У операции отсутствует имя файла.")?;
        Ok(Some(self.root.join(folder).join(name)))
    }

    fn prepare(&self, plan: &[PlannedFileOperation], backup: &mut BackupSession) -> AppResult<Vec<Option<PathBuf>>> {
        let mut staged = Vec::with_capacity(plan.len());
        for operation in plan {
            validate_target(operation)?;
            let Some(path) = self.file_for(operation)? else {
                staged.push(None);
                continue;
            };
            let bytes = operation.content.as_ref().ok_or("Для операции создания или замены отсутствует содержимое.")?;
            let mut file = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path)
                .map_err(|error| format!("Не удалось создать staging-файл {}: {error}", path.display()))?;
            file.write_all(bytes)?;
            file.sync_all()?;
            drop(file);
            if fs::read(&path)? != *bytes {
                return Err(format!("Побайтовая проверка staging-файла не пройдена: {}", path.display()).into());
            }
            backup.event("STAGING", &format!("Подготовлен и проверен файл: {}.", path.display()))?;
            staged.push(Some(path));
        }
        Ok(staged)
    }

    fn cleanup(&self, backup: &mut BackupSession) -> AppResult<()> {
        match fs::remove_dir_all(&self.root) {
            Ok(()) => backup.event("STAGING", "Временный staging-каталог удалён."),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => {
                backup.event("ERROR", &format!("Не удалось удалить staging-каталог {}: {error}", self.root.display()))?;
                Err(error.into())
            }
        }
    }
}

impl Drop for StagingDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

pub(crate) fn execute(plan: &[PlannedFileOperation], backup: &mut BackupSession) -> AppResult<()> {
    if plan.is_empty() {
        backup.event("APPLY", "План пуст: изменения файлов не требуются; перезапуск XKeen пропущен.")?;
        return Ok(());
    }

    let staging = StagingDirectory::create()?;
    let staged = match staging.prepare(plan, backup) {
        Ok(staged) => staged,
        Err(error) => {
            backup.event("ERROR", &format!("Подготовка staging завершилась ошибкой; применение не начиналось: {error}"))?;
            let _ = staging.cleanup(backup);
            return Err(error);
        }
    };

    let mut order: Vec<usize> = (0..plan.len()).collect();
    order.sort_by_key(|index| (category_order(plan[*index].category), plan[*index].path.clone()));
    let mut applied = Vec::new();

    let mut baseline_directory_created = false;
    for index in order {
        if plan[index].category == "Baseline" && !baseline_directory_created
            && !Path::new(BASELINE_ROOT).exists()
        {
            match create_baseline_directory(backup, &mut baseline_directory_created) {
                Ok(()) => {}
                Err(error) => {
                    backup.event("ERROR", &format!("Не удалось подготовить каталог baseline: {error}")).ok();
                    let rollback = rollback(plan, &staged, &applied, backup);
                    cleanup_created_baseline_directory(baseline_directory_created, backup);
                    let restart = start_after_rollback(backup);
                    let _ = staging.cleanup(backup);
                    return Err(format!("{error}; откат: {}; запуск XKeen после отката: {}", rollback, restart).into());
                }
            }
        }
        if matches!(plan[index].kind, PlannedOperationKind::Replace | PlannedOperationKind::Delete) {
            match backup.verify_original(&plan[index].path) {
                Ok(true) => {}
                Ok(false) => {
                    let message = format!("Исходный файл изменился после резервирования; применение остановлено: {}", plan[index].path.display());
                    backup.event("ERROR", &message).ok();
                    let rollback = rollback(plan, &staged, &applied, backup);
                    cleanup_created_baseline_directory(baseline_directory_created, backup);
                    let restart = start_after_rollback(backup);
                    let _ = staging.cleanup(backup);
                    return Err(format!("{message}; откат: {}; запуск XKeen после отката: {}", rollback, restart).into());
                }
                Err(error) => {
                    let message = format!("Не удалось проверить исходный файл перед заменой {}: {error}", plan[index].path.display());
                    backup.event("ERROR", &message).ok();
                    let rollback = rollback(plan, &staged, &applied, backup);
                    cleanup_created_baseline_directory(baseline_directory_created, backup);
                    let restart = start_after_rollback(backup);
                    let _ = staging.cleanup(backup);
                    return Err(format!("{message}; откат: {}; запуск XKeen после отката: {}", rollback, restart).into());
                }
            }
        }
        match apply_one(&plan[index], staged[index].as_deref()) {
            Ok(()) => {
                applied.push(index);
                if let Err(log_error) = backup.event("APPLY", &format!("Операция применена: {:?} {}.", plan[index].kind, plan[index].path.display())) {
                    let message = format!("Не удалось записать результат применения {} в журнал: {log_error}", plan[index].path.display());
                    let rollback = rollback(plan, &staged, &applied, backup);
                    cleanup_created_baseline_directory(baseline_directory_created, backup);
                    let restart = start_after_rollback(backup);
                    let _ = staging.cleanup(backup);
                    return Err(format!("{message}; откат: {}; запуск XKeen после отката: {}", rollback, restart).into());
                }
            }
            Err((message, mutated)) => {
                if mutated { applied.push(index); }
                backup.event("ERROR", &format!("Ошибка применения {}: {message}", plan[index].path.display())).ok();
                let rollback = rollback(plan, &staged, &applied, backup);
                cleanup_created_baseline_directory(baseline_directory_created, backup);
                let restart = start_after_rollback(backup);
                let _ = staging.cleanup(backup);
                return Err(format!("{message}; откат: {}; запуск XKeen после отката: {}", rollback, restart).into());
            }
        }
    }

    if let Err(error) = verify_applied(plan, &staged, backup) {
        backup.event("ERROR", &format!("Контроль итогового состояния файлов не пройден: {error}")).ok();
        let rollback = rollback(plan, &staged, &applied, backup);
        cleanup_created_baseline_directory(baseline_directory_created, backup);
        let restart = start_after_rollback(backup);
        let _ = staging.cleanup(backup);
        return Err(format!("{error}; откат: {}; запуск XKeen после отката: {}", rollback, restart).into());
    }

    if let Err(error) = restart_xkeen(backup) {
        backup.event("ERROR", &format!("Перезапуск или проверка XKeen завершились ошибкой: {error}")).ok();
        let rollback = rollback(plan, &staged, &applied, backup);
        cleanup_created_baseline_directory(baseline_directory_created, backup);
        let restart = start_after_rollback(backup);
        let _ = staging.cleanup(backup);
        return Err(format!("{error}; откат: {}; запуск XKeen после отката: {}", rollback, restart).into());
    }

    if let Err(log_error) = backup.event("APPLY", "Все файлы применены и проверены; XKeen запущен, статус подтверждён.") {
        let rollback = rollback(plan, &staged, &applied, backup);
        cleanup_created_baseline_directory(baseline_directory_created, backup);
        let restart = start_after_rollback(backup);
        let _ = staging.cleanup(backup);
        return Err(format!("Не удалось записать итог успешного применения в журнал: {log_error}; откат: {}; запуск XKeen после отката: {}", rollback, restart).into());
    }
    if let Err(error) = staging.cleanup(backup) {
        backup.event("WARNING", &format!("Обновление применено, но staging-каталог не удалось удалить: {error}")).ok();
    }
    Ok(())
}

fn create_baseline_directory(backup: &mut BackupSession, created: &mut bool) -> AppResult<()> {
    let path = Path::new(BASELINE_ROOT);
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(format!("Путь baseline не является обычным каталогом: {}", path.display()).into());
        }
        Ok(_) => return Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    fs::create_dir(path)?;
    *created = true;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    backup.event("APPLY", &format!("Создан каталог baseline: {}.", path.display()))?;
    Ok(())
}

fn cleanup_created_baseline_directory(created: bool, backup: &mut BackupSession) {
    if !created { return; }
    let path = Path::new(BASELINE_ROOT);
    match fs::remove_dir(path) {
        Ok(()) => { backup.event("ROLLBACK", &format!("Удалён пустой каталог baseline, созданный текущим update: {}.", path.display())).ok(); }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            backup.event("ROLLBACK", &format!("Не удалось удалить каталог baseline {} после отката (возможно, он не пуст): {error}", path.display())).ok();
        }
    }
}

fn category_order(category: &str) -> u8 {
    match category {
        "Конфигурация Xray" => 0,
        "Архив пользовательских подключений" => 1,
        "Baseline" | "Повреждённый baseline" => 2,
        _ => 3,
    }
}

fn validate_target(operation: &PlannedFileOperation) -> AppResult<()> {
    let path = &operation.path;
    if !path.is_absolute() || path.components().any(|part| matches!(part, std::path::Component::ParentDir)) {
        return Err(format!("Недопустимый абсолютный путь операции: {}", path.display()).into());
    }
    let allowed = match operation.category {
        "Конфигурация Xray" => path.starts_with(CONFIG_ROOT),
        "Архив пользовательских подключений" => path == Path::new(ARCHIVE_PATH),
        "Baseline" | "Повреждённый baseline" => path.starts_with(BASELINE_ROOT),
        _ => false,
    };
    if !allowed {
        return Err(format!("Путь не соответствует категории операции: {}", path.display()).into());
    }
    match fs::symlink_metadata(path) {
        Ok(metadata) if operation.kind == PlannedOperationKind::Create => {
            return Err(format!("Файл для операции Create уже существует: {}", path.display()).into());
        }
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(format!("Целевой путь не является обычным файлом: {}", path.display()).into());
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("Не удалось проверить целевой путь {}: {error}", path.display()).into()),
    }
    let parent = path.parent().ok_or("У целевого файла отсутствует родительский каталог.")?;
    match fs::symlink_metadata(parent) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(format!("Каталог назначения является небезопасным объектом: {}", parent.display()).into());
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound
            && operation.category == "Baseline"
            && operation.kind == PlannedOperationKind::Create
            && parent == Path::new(BASELINE_ROOT) => {}
        Err(error) => return Err(format!("Каталог назначения не существует или недоступен {}: {error}", parent.display()).into()),
    }
    Ok(())
}

fn apply_one(operation: &PlannedFileOperation, staged: Option<&Path>) -> Result<(), (String, bool)> {
    match operation.kind {
        PlannedOperationKind::Delete => {
            match fs::remove_file(&operation.path) {
                Ok(()) => {
                    match is_path_absent(&operation.path) {
                        Ok(true) => Ok(()),
                        Ok(false) => Err((format!("Файл не исчез после удаления: {}", operation.path.display()), true)),
                        Err(error) => Err((format!("Не удалось проверить удаление {}: {error}", operation.path.display()), true)),
                    }
                }
                Err(error) => Err((
                    format!("Не удалось удалить {}: {error}", operation.path.display()),
                    error.kind() == std::io::ErrorKind::NotFound,
                )),
            }
        }
        PlannedOperationKind::Create | PlannedOperationKind::Replace => {
            let staged = staged.ok_or_else(|| ("Не найден staging-файл.".to_owned(), false))?;
            let expected = operation.content.as_deref().ok_or_else(|| ("Не задано содержимое операции.".to_owned(), false))?;
            let parent = operation.path.parent().ok_or_else(|| ("Не определён каталог назначения.".to_owned(), false))?;
            let name = operation.path.file_name().and_then(|value| value.to_str()).ok_or_else(|| ("Недопустимое имя целевого файла.".to_owned(), false))?;
            let counter = crate::temp_counter::next();
            let temporary = parent.join(format!(".{name}.xkeen-sub-{}-{counter}.tmp", std::process::id()));
            let mut temporary_created = false;
            let mut destination_mutated = false;
            let result = (|| -> AppResult<()> {
                let mut file = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temporary)?;
                temporary_created = true;
                let mut input = fs::File::open(staged)?;
                std::io::copy(&mut input, &mut file)?;
                file.sync_all()?;
                drop(file);
                if fs::read(&temporary)? != expected {
                    return Err(format!("Побайтовая проверка временного файла не пройдена: {}", temporary.display()).into());
                }
                let (expected_mode, expected_uid, expected_gid) = if operation.kind == PlannedOperationKind::Replace {
                    let original = fs::symlink_metadata(&operation.path)?;
                    if original.file_type().is_symlink() || !original.is_file() {
                        return Err(format!("Исходный путь стал небезопасным объектом: {}", operation.path.display()).into());
                    }
                    let mode = original.mode() & 0o7777;
                    set_owner(&temporary, original.uid(), original.gid())?;
                    fs::set_permissions(&temporary, fs::Permissions::from_mode(mode))?;
                    (mode, original.uid(), original.gid())
                } else {
                    set_owner(&temporary, 0, 0)?;
                    fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))?;
                    (0o600, 0, 0)
                };
                if operation.kind == PlannedOperationKind::Create {
                    // A hard link in the same directory provides atomic no-clobber creation.
                    fs::hard_link(&temporary, &operation.path)?;
                    destination_mutated = true;
                    fs::remove_file(&temporary)?;
                } else {
                    fs::rename(&temporary, &operation.path)?;
                    destination_mutated = true;
                }
                if fs::read(&operation.path)? != expected {
                    return Err(format!("Побайтовая проверка применённого файла не пройдена: {}", operation.path.display()).into());
                }
                let applied_metadata = fs::symlink_metadata(&operation.path)?;
                if applied_metadata.file_type().is_symlink()
                    || applied_metadata.mode() & 0o7777 != expected_mode
                    || applied_metadata.uid() != expected_uid
                    || applied_metadata.gid() != expected_gid
                {
                    return Err(format!("Проверка владельца или прав применённого файла не пройдена: {}", operation.path.display()).into());
                }
                Ok(())
            })();
            match result {
                Ok(()) => Ok(()),
                Err(error) => {
                    // Track the successful atomic operation, not merely the existence of
                    // a destination that may have been created by another process.
                    let mutated = destination_mutated
                        || (operation.kind == PlannedOperationKind::Replace
                            && is_path_absent(&operation.path).unwrap_or(false));
                    if temporary_created {
                        if let Err(cleanup) = fs::remove_file(&temporary) {
                            if cleanup.kind() != std::io::ErrorKind::NotFound {
                                return Err((format!("{error}; не удалось удалить временный файл {}: {cleanup}", temporary.display()), mutated));
                            }
                        }
                    }
                    Err((error.to_string(), mutated))
                }
            }
        }
    }
}

fn is_path_absent(path: &Path) -> std::io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(false),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(error),
    }
}

fn set_owner(path: &Path, uid: u32, gid: u32) -> AppResult<()> {
    let output = Command::new("chown").arg(format!("{uid}:{gid}")).arg("--").arg(path).output()
        .map_err(|error| format!("Не удалось запустить chown для {}: {error}", path.display()))?;
    if !output.status.success() {
        return Err(format!("chown завершился с кодом {:?}: {}", output.status.code(), String::from_utf8_lossy(&output.stderr)).into());
    }
    Ok(())
}

fn verify_applied(plan: &[PlannedFileOperation], staged: &[Option<PathBuf>], backup: &mut BackupSession) -> AppResult<()> {
    for (index, operation) in plan.iter().enumerate() {
        match operation.kind {
            PlannedOperationKind::Delete => {
                if !is_path_absent(&operation.path)? {
                    return Err(format!("Файл, запланированный к удалению, существует: {}", operation.path.display()).into());
                }
            }
            PlannedOperationKind::Create | PlannedOperationKind::Replace => {
                let metadata = fs::symlink_metadata(&operation.path)?;
                if metadata.file_type().is_symlink() || !metadata.is_file() {
                    return Err(format!("Итоговый путь стал небезопасным объектом: {}", operation.path.display()).into());
                }
                let expected = fs::read(staged[index].as_ref().ok_or("Не найден staging-файл при проверке.")?)?;
                if fs::read(&operation.path)? != expected {
                    return Err(format!("Фактическое содержимое отличается от staging: {}", operation.path.display()).into());
                }
            }
        }
        backup.event("VERIFY", &format!("Итоговое состояние подтверждено: {}.", operation.path.display()))?;
    }
    Ok(())
}

fn rollback(
    plan: &[PlannedFileOperation],
    staged: &[Option<PathBuf>],
    applied: &[usize],
    backup: &mut BackupSession,
) -> String {
    backup.event("ROLLBACK", &format!("Начат откат. Операций для обработки: {}.", applied.len())).ok();
    let mut restored = 0usize;
    let mut failed = 0usize;
    for index in applied.iter().rev().copied() {
        let operation = &plan[index];
        let result = match operation.kind {
            PlannedOperationKind::Replace | PlannedOperationKind::Delete => {
                retry_restore(&operation.path, backup)
            }
            PlannedOperationKind::Create => {
                match staged.get(index).and_then(Option::as_ref) {
                    Some(staged_path) => match fs::read(staged_path) {
                        Ok(expected) => retry_remove_created(&operation.path, &expected, backup),
                        Err(error) => Err(format!("Не удалось прочитать staging-файл для безопасного удаления {}: {error}", operation.path.display())),
                    },
                    None => Err(format!("Для созданного файла отсутствует staging-файл: {}", operation.path.display())),
                }
            }
        };
        match result {
            Ok(changed) => {
                if changed { restored += 1; }
                backup.event("ROLLBACK", &format!(
                    "{}: {}.",
                    if changed { "Откат операции подтверждён" } else { "Операция уже не требовала восстановления" },
                    operation.path.display()
                )).ok();
            }
            Err(error) => {
                failed += 1;
                backup.event("ROLLBACK", &format!("Не удалось откатить {}: {error}", operation.path.display())).ok();
            }
        }
    }
    let mut final_failures = 0usize;
    for index in applied {
        let operation = &plan[*index];
        let verified = match operation.kind {
            PlannedOperationKind::Replace | PlannedOperationKind::Delete => backup.verify_original(&operation.path).unwrap_or(false),
            PlannedOperationKind::Create => is_path_absent(&operation.path).unwrap_or(false),
        };
        if !verified {
            final_failures += 1;
            backup.event("ROLLBACK", &format!("Итоговая проверка отката не пройдена: {}.", operation.path.display())).ok();
        }
    }
    for index in applied {
        let operation = &plan[*index];
        if let Some(parent) = operation.path.parent() {
            if let Ok(entries) = fs::read_dir(parent) {
                let prefix = operation.path.file_name().and_then(|name| name.to_str())
                    .map(|name| format!(".{name}.xkeen-sub-"));
                if let Some(prefix) = prefix {
                    for entry in entries.flatten() {
                        let name = entry.file_name().to_string_lossy().into_owned();
                        if name.starts_with(&prefix) && name.ends_with(".tmp") {
                            backup.event("ROLLBACK", &format!(
                                "Обнаружен оставшийся временный файл; не удалён автоматически: {}",
                                entry.path().display()
                            )).ok();
                        }
                    }
                }
            }
        }
    }
    let status = if restored == 0 {
        "не выполнен"
    } else if failed > 0 || final_failures > 0 {
        "частичный"
    } else {
        "успешный"
    };
    backup.event("ROLLBACK", &format!("Итог отката: {status}; восстановлено/удалено: {restored}; ошибок: {}; ошибок итоговой проверки: {final_failures}.", failed)).ok();
    format!("{status} (успешно: {restored}, ошибок: {})", failed + final_failures)
}

fn retry_restore(path: &Path, backup: &mut BackupSession) -> Result<bool, String> {
    for attempt in 1..=MAX_FILE_ATTEMPTS {
        backup.event("ROLLBACK", &format!("Попытка {attempt}/{MAX_FILE_ATTEMPTS} восстановить {}.", path.display())).ok();
        match backup.restore_original(path) {
            Ok(()) => return Ok(true),
            Err(error) => {
                backup.event("ROLLBACK", &format!("Попытка {attempt} не удалась для {}: {error}", path.display())).ok();
                if attempt == MAX_FILE_ATTEMPTS { return Err(error.to_string()); }
            }
        }
    }
    Err("Исчерпаны попытки восстановления.".to_owned())
}

fn retry_remove_created(path: &Path, expected: &[u8], backup: &mut BackupSession) -> Result<bool, String> {
    for attempt in 1..=MAX_FILE_ATTEMPTS {
        backup.event("ROLLBACK", &format!("Попытка {attempt}/{MAX_FILE_ATTEMPTS} удалить созданный файл {}.", path.display())).ok();
        if let Ok(metadata) = fs::symlink_metadata(path) {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(format!("Конфликт: созданный файл заменён небезопасным объектом; удаление запрещено: {}", path.display()));
            }
        }
        match fs::read(path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => {
                if attempt == MAX_FILE_ATTEMPTS { return Err(error.to_string()); }
            }
            Ok(current) if current != expected => {
                return Err(format!("Конфликт: содержимое изменено вне текущего update; файл не удалён: {}", path.display()));
            }
            Ok(_) => match fs::remove_file(path) {
                Ok(()) if is_path_absent(path).unwrap_or(false) => return Ok(true),
                Ok(()) => {
                    if attempt == MAX_FILE_ATTEMPTS { return Err("Файл остался после удаления.".to_owned()); }
                }
                Err(error) if attempt == MAX_FILE_ATTEMPTS => return Err(error.to_string()),
                Err(_) => {}
            }
        }
    }
    Err("Исчерпаны попытки удаления.".to_owned())
}

struct CommandResult {
    code: Option<i32>,
    timed_out: bool,
}

fn run_xkeen(args: &[&str], backup: &mut BackupSession) -> AppResult<CommandResult> {
    let command_text = format!("XKEEN_FOREGROUND=1 xkeen {}", args.join(" "));
    let started = timestamp();
    backup.event("COMMAND", &format!("Команда: {command_text}; начало: {started}."))?;
    // Не захватываем stdout/stderr: команды XKeen могут запускать фоновые
    // процессы, которые наследуют файловые дескрипторы родителя.
    let mut child = match Command::new("xkeen").args(args).env("XKEEN_FOREGROUND", "1")
        .stdout(Stdio::null()).stderr(Stdio::null()).spawn() {
        Ok(child) => child,
        Err(error) => {
            backup.event("COMMAND", &format!("Команда: {command_text}; ошибка запуска: {error}; завершение: {}.", timestamp()))?;
            return Err(format!("Не удалось запустить {command_text}: {error}").into());
        }
    };
    let deadline = Instant::now() + COMMAND_TIMEOUT;
    let (status, timed_out) = loop {
        if let Some(status) = child.try_wait()? { break (Some(status), false); }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let status = child.wait().ok();
            break (status, true);
        }
        thread::sleep(Duration::from_millis(100));
    };
    let code = status.as_ref().and_then(|value| value.code());
    backup.event("COMMAND", &format!(
        "Команда: {command_text}; завершение: {}; exit_code: {:?}; timeout: {}.",
        timestamp(), code, timed_out
    ))?;
    Ok(CommandResult { code, timed_out })
}

// Повторяет критерии proxy_pids() из XKeen:
// учитывается только процесс xray с рабочей командой, а одноразовые
// test/dump и help/version исключаются.
fn is_xray_proxy_cmdline(cmdline: &[u8]) -> bool {
    let mut args: Vec<&[u8]> = cmdline.split(|byte| *byte == 0).collect();
    while args.last().is_some_and(|arg| arg.is_empty()) {
        args.pop();
    }
    if args.is_empty() {
        return false;
    }

    let one_shot = args.iter().skip(1).any(|arg| {
        matches!(*arg, b"-test" | b"--test" | b"-test=true" | b"--test=true"
            | b"-dump" | b"--dump" | b"-dump=true" | b"--dump=true")
    });
    if one_shot {
        return false;
    }

    if args.len() == 1 {
        return true;
    }

    let command = args[1];
    command == b"run"
        || (command.starts_with(b"-")
            && !matches!(command, b"-h" | b"--h" | b"-help" | b"--help" | b"-version" | b"--version"))
}

fn xray_process_running() -> std::io::Result<bool> {
    for entry in fs::read_dir("/proc")? {
        let Ok(entry) = entry else { continue };
        if entry.file_name().to_string_lossy().parse::<u32>().is_err() {
            continue;
        }
        let process_dir = entry.path();
        let comm = match fs::read_to_string(process_dir.join("comm")) {
            Ok(comm) => comm,
            Err(_) => continue, // Процесс мог завершиться во время обхода.
        };
        if comm.trim_end() != "xray" {
            continue;
        }
        let cmdline = match fs::read(process_dir.join("cmdline")) {
            Ok(cmdline) => cmdline,
            Err(_) => continue,
        };
        if is_xray_proxy_cmdline(&cmdline) {
            return Ok(true);
        }
    }
    Ok(false)
}

const STATUS_POLL_TIMEOUT: Duration = Duration::from_secs(10);
const STATUS_POLL_INTERVAL: Duration = Duration::from_millis(250);

fn wait_for_stopped_status(backup: &mut BackupSession) -> AppResult<bool> {
    let deadline = Instant::now() + STATUS_POLL_TIMEOUT;
    loop {
        let running = xray_process_running()
            .map_err(|error| format!("Не удалось проверить процессы Xray в /proc: {error}"))?;
        if !running {
            backup.event("XKEEN", "Проверка процесса: рабочий процесс Xray остановлен.")?;
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        thread::sleep(STATUS_POLL_INTERVAL);
    }
}

fn wait_for_running_status(backup: &mut BackupSession) -> AppResult<bool> {
    let deadline = Instant::now() + STATUS_POLL_TIMEOUT;
    loop {
        let running = xray_process_running()
            .map_err(|error| format!("Не удалось проверить процессы Xray в /proc: {error}"))?;
        if running {
            backup.event("XKEEN", "Проверка процесса: рабочий процесс Xray запущен.")?;
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        thread::sleep(STATUS_POLL_INTERVAL);
    }
}

fn restart_xkeen(backup: &mut BackupSession) -> AppResult<()> {
    let stop = run_xkeen(&["-stop"], backup);
    let stopped = wait_for_stopped_status(backup)?;
    let stop_description = match &stop {
        Ok(result) => format!("exit_code={:?}, timeout={}", result.code, result.timed_out),
        Err(error) => format!("ошибка запуска команды: {error}"),
    };
    backup.event("XKEEN", &format!(
        "Проверка остановки: рабочий процесс Xray остановлен={stopped}; {stop_description}"
    ))?;
    if !stopped {
        return Err(format!(
            "Остановка XKeen не подтверждена: рабочий процесс Xray всё ещё обнаруживается; {stop_description}"
        ).into());
    }

    let start = run_xkeen(&["-start"], backup);
    let running = wait_for_running_status(backup)?;
    let start_description = match &start {
        Ok(result) => format!("exit_code={:?}, timeout={}", result.code, result.timed_out),
        Err(error) => format!("ошибка запуска команды: {error}"),
    };
    backup.event("XKEEN", &format!(
        "Проверка запуска: рабочий процесс Xray запущен={running}; {start_description}."
    ))?;
    if !running {
        return Err(format!(
            "Запуск XKeen не подтверждён: рабочий процесс Xray не обнаружен; {start_description}"
        ).into());
    }
    backup.event("XKEEN", "Остановка, запуск и проверка рабочего процесса Xray завершились успешно.")?;
    Ok(())
}

fn start_after_rollback(backup: &mut BackupSession) -> String {
    let start = run_xkeen(&["-start"], backup);
    let status = wait_for_running_status(backup);

    let running = matches!(status, Ok(true));
    let start_description = match &start {
        Ok(result) => format!("exit_code={:?}, timeout={}", result.code, result.timed_out),
        Err(error) => format!("ошибка запуска команды: {error}"),
    };
    let status_description = match &status {
        Ok(true) => "рабочий процесс Xray обнаружен".to_owned(),
        Ok(false) => "рабочий процесс Xray не обнаружен по истечении тайм-аута".to_owned(),
        Err(error) => format!("ошибка проверки процессов: {error}"),
    };
    backup.event("ROLLBACK", &format!(
        "Результат запуска XKeen после отката: рабочий процесс Xray запущен={running}; {start_description}; {status_description}"
    )).ok();

    if running {
        "запущен, процесс Xray подтверждён".to_owned()
    } else {
        let message = format!(
            "Не удалось подтвердить запуск XKeen после отката: {start_description}; {status_description}"
        );
        backup.event("ERROR", &message).ok();
        message
    }
}


const STATUS_POLL_TIMEOUT: Duration = Duration::from_secs(10);
const STATUS_POLL_INTERVAL: Duration = Duration::from_millis(250);

fn wait_for_stopped_status(backup: &mut BackupSession) -> AppResult<CommandResult> {
    let deadline = Instant::now() + STATUS_POLL_TIMEOUT;
    let mut last_status = None;
    loop {
        let status = run_xkeen(&["-status"], backup)?;
        let stopped = !status.timed_out && is_stopped_status(status.code, &status.stdout);
        if stopped {
            return Ok(status);
        }
        last_status = Some(status);
        if Instant::now() >= deadline {
            return Ok(last_status.expect("at least one status check was performed"));
        }
        thread::sleep(STATUS_POLL_INTERVAL);
    }
}

fn wait_for_running_status(backup: &mut BackupSession) -> AppResult<CommandResult> {
    let deadline = Instant::now() + STATUS_POLL_TIMEOUT;
    let mut last_status = None;
    loop {
        let status = run_xkeen(&["-status"], backup)?;
        let running = !status.timed_out && is_running_status(status.code, &status.stdout);
        if running {
            return Ok(status);
        }
        last_status = Some(status);
        if Instant::now() >= deadline {
            return Ok(last_status.expect("at least one status check was performed"));
        }
        thread::sleep(STATUS_POLL_INTERVAL);
    }
}

fn restart_xkeen(backup: &mut BackupSession) -> AppResult<()> {
    let stop = run_xkeen(&["-stop"], backup);
    // The status command is authoritative: the stop command may return before
    // XKeen has finished stopping.
    let status = wait_for_stopped_status(backup);
    let stop_description = match &stop {
        Ok(result) => format!("exit_code={:?}, timeout={}", result.code, result.timed_out),
        Err(error) => format!("ошибка запуска команды: {error}"),
    };
    let status_description = match &status {
        Ok(result) => format!("exit_code={:?}, timeout={}, stdout={}", result.code, result.timed_out, result.stdout),
        Err(error) => format!("ошибка проверки статуса: {error}"),
    };
    let stopped = matches!(&status, Ok(result) if !result.timed_out && is_stopped_status(result.code, &result.stdout));
    backup.event("XKEEN", &format!(
        "Проверка остановки: состояние остановлено={stopped}; {stop_description}; {status_description}"
    ))?;
    if !stopped {
        return Err(format!(
            "Остановка XKeen не подтверждена: {stop_description}; {status_description}"
        ).into());
    }

    let start = run_xkeen(&["-start"], backup);
    // The final status is authoritative; routing mode does not determine
    // whether the proxy process is running.
    let status = wait_for_running_status(backup)?;
    let running = !status.timed_out && is_running_status(status.code, &status.stdout);
    let start_description = match &start {
        Ok(result) => format!("exit_code={:?}, timeout={}", result.code, result.timed_out),
        Err(error) => format!("ошибка запуска команды: {error}"),
    };
    backup.event("XKEEN", &format!(
        "Проверка запуска: состояние запущено={running}; {start_description}; status_code={:?}; stdout={}.",
        status.code, status.stdout
    ))?;
    if !running {
        return Err(format!(
            "Запуск XKeen не подтверждён: {start_description}; exit_code={:?}; stdout={}",
            status.code, status.stdout
        ).into());
    }
    backup.event("XKEEN", "Остановка, запуск и проверка статуса XKeen завершились успешно.")?;
    Ok(())
}

fn start_after_rollback(backup: &mut BackupSession) -> String {
    let start = run_xkeen(&["-start"], backup);
    // The final status is authoritative; routing mode does not determine
    // whether the proxy process is running.
    let status = wait_for_running_status(backup);

    let running = matches!(&status, Ok(result) if !result.timed_out && is_running_status(result.code, &result.stdout));
    let start_description = match &start {
        Ok(result) => format!("exit_code={:?}, timeout={}", result.code, result.timed_out),
        Err(error) => format!("ошибка запуска команды: {error}"),
    };
    let status_description = match &status {
        Ok(result) => format!("exit_code={:?}, stdout={}", result.code, result.stdout),
        Err(error) => format!("ошибка проверки статуса: {error}"),
    };
    backup.event("ROLLBACK", &format!(
        "Результат запуска XKeen после отката: состояние запущено={running}; {start_description}; {status_description}"
    )).ok();

    if running {
        "запущен, статус подтверждён".to_owned()
    } else {
        let message = format!(
            "Не удалось подтвердить запуск XKeen после отката: состояние запущено={running}; {start_description}; {status_description}"
        );
        backup.event("ERROR", &message).ok();
        message
    }
}

fn is_stopped_status(code: Option<i32>, stdout: &str) -> bool {
    code == Some(1) && stdout.to_lowercase().contains("прокси-клиент не запущен")
}

fn is_running_status(code: Option<i32>, stdout: &str) -> bool {
    let normalized = stdout.to_lowercase();
    code == Some(0)
        && normalized.contains("прокси-клиент")
        && normalized.contains("запущен")
        && !normalized.contains("не запущен")
}

#[cfg(test)]
mod status_tests {
    use super::{is_running_status, is_stopped_status};

    #[test]
    fn captures_output_only_for_status_commands() {
        use super::should_capture_output;

        assert!(should_capture_output(&["-status"]));
        assert!(!should_capture_output(&["-start"]));
        assert!(!should_capture_output(&["-stop"]));
    }

    #[test]
    fn accepts_only_explicit_stopped_status_with_expected_exit_code() {
        assert!(is_stopped_status(Some(1), "Прокси-клиент не запущен"));
        assert!(!is_stopped_status(Some(0), "Прокси-клиент не запущен"));
        assert!(!is_stopped_status(Some(1), "не удалось определить статус"));
    }

    #[test]
    fn accepts_all_documented_xkeen_proxy_modes_as_running() {
        for mode in ["TProxy", "Hybrid", "Redirect", "Other", "FutureMode"] {
            let output = format!("Прокси-клиент xray запущен в режиме {mode}");
            assert!(is_running_status(Some(0), &output), "mode {mode} must not affect process state");
        }
    }

    #[test]
    fn accepts_legacy_mixed_mode_name_as_running() {
        assert!(is_running_status(Some(0), "Прокси-клиент xray запущен в режиме Mixed"));
    }

    #[test]
    fn rejects_nonzero_or_negative_running_status() {
        assert!(is_running_status(Some(0), "Прокси-клиент запущен"));
        assert!(!is_running_status(Some(1), "Прокси-клиент запущен"));
        assert!(!is_running_status(Some(0), "Прокси-клиент не запущен"));
        assert!(!is_running_status(Some(0), "Прокси-клиент НЕ запущен"));
        assert!(!is_running_status(Some(0), "не удалось определить статус"));
    }
}

fn timestamp() -> String {
    Command::new("date").arg("+%Y-%m-%d %H:%M:%S").output()
        .ok().filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .unwrap_or_else(|| SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs().to_string()).unwrap_or_else(|_| "unknown".to_owned()))
}
