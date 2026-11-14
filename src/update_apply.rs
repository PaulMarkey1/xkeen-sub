use crate::backup::BackupSession;
use std::error::Error;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
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

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

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

    for index in order {
        if matches!(plan[index].kind, PlannedOperationKind::Replace | PlannedOperationKind::Delete) {
            match backup.verify_original(&plan[index].path) {
                Ok(true) => {}
                Ok(false) => {
                    let message = format!("Исходный файл изменился после резервирования; применение остановлено: {}", plan[index].path.display());
                    backup.event("ERROR", &message)?;
                    let rollback = rollback(plan, &staged, &applied, backup);
                    let restart = start_after_rollback(backup);
                    let _ = staging.cleanup(backup);
                    return Err(format!("{message}; откат: {}; запуск XKeen после отката: {}", rollback, restart).into());
                }
                Err(error) => {
                    let message = format!("Не удалось проверить исходный файл перед заменой {}: {error}", plan[index].path.display());
                    backup.event("ERROR", &message)?;
                    let rollback = rollback(plan, &staged, &applied, backup);
                    let restart = start_after_rollback(backup);
                    let _ = staging.cleanup(backup);
                    return Err(format!("{message}; откат: {}; запуск XKeen после отката: {}", rollback, restart).into());
                }
            }
        }
        match apply_one(&plan[index], staged[index].as_deref()) {
            Ok(()) => {
                applied.push(index);
                backup.event("APPLY", &format!("Операция применена: {:?} {}.", plan[index].kind, plan[index].path.display()))?;
            }
            Err((message, mutated)) => {
                if mutated { applied.push(index); }
                backup.event("ERROR", &format!("Ошибка применения {}: {message}", plan[index].path.display()))?;
                let rollback = rollback(plan, &staged, &applied, backup);
                let restart = start_after_rollback(backup);
                let _ = staging.cleanup(backup);
                return Err(format!("{message}; откат: {}; запуск XKeen после отката: {}", rollback, restart).into());
            }
        }
    }

    if let Err(error) = verify_applied(plan, &staged, backup) {
        backup.event("ERROR", &format!("Контроль итогового состояния файлов не пройден: {error}"))?;
        let rollback = rollback(plan, &staged, &applied, backup);
        let restart = start_after_rollback(backup);
        let _ = staging.cleanup(backup);
        return Err(format!("{error}; откат: {}; запуск XKeen после отката: {}", rollback, restart).into());
    }

    if let Err(error) = restart_xkeen(backup) {
        backup.event("ERROR", &format!("Перезапуск или проверка XKeen завершились ошибкой: {error}"))?;
        let rollback = rollback(plan, &staged, &applied, backup);
        let restart = start_after_rollback(backup);
        let _ = staging.cleanup(backup);
        return Err(format!("{error}; откат: {}; запуск XKeen после отката: {}", rollback, restart).into());
    }

    backup.event("APPLY", "Все файлы применены и проверены; XKeen запущен, статус подтверждён.")?;
    if let Err(error) = staging.cleanup(backup) {
        backup.event("WARNING", &format!("Обновление применено, но staging-каталог не удалось удалить: {error}"))?;
    }
    Ok(())
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
    if !path.parent().ok_or("У целевого файла отсутствует родительский каталог.")?.is_dir() {
        return Err(format!("Каталог назначения не существует: {}", path.parent().unwrap().display()).into());
    }
    Ok(())
}

fn apply_one(operation: &PlannedFileOperation, staged: Option<&Path>) -> Result<(), (String, bool)> {
    match operation.kind {
        PlannedOperationKind::Delete => {
            match fs::remove_file(&operation.path) {
                Ok(()) => {
                    if operation.path.exists() {
                        Err((format!("Файл не исчез после удаления: {}", operation.path.display()), true))
                    } else { Ok(()) }
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
            let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let temporary = parent.join(format!(".{name}.xkeen-sub-{}-{counter}.tmp", std::process::id()));
            let result = (|| -> AppResult<()> {
                let mut file = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temporary)?;
                let mut input = fs::File::open(staged)?;
                std::io::copy(&mut input, &mut file)?;
                file.sync_all()?;
                drop(file);
                if fs::read(&temporary)? != expected {
                    return Err(format!("Побайтовая проверка временного файла не пройдена: {}", temporary.display()).into());
                }
                if operation.kind == PlannedOperationKind::Replace {
                    let original = fs::metadata(&operation.path)?;
                    set_owner(&temporary, original.uid(), original.gid())?;
                    fs::set_permissions(&temporary, fs::Permissions::from_mode(original.mode() & 0o7777))?;
                } else {
                    set_owner(&temporary, 0, 0)?;
                    fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))?;
                }
                if operation.kind == PlannedOperationKind::Create {
                    // A hard link in the same directory provides atomic no-clobber creation.
                    fs::hard_link(&temporary, &operation.path)?;
                    fs::remove_file(&temporary)?;
                } else {
                    fs::rename(&temporary, &operation.path)?;
                }
                if fs::read(&operation.path)? != expected {
                    return Err(format!("Побайтовая проверка применённого файла не пройдена: {}", operation.path.display()).into());
                }
                Ok(())
            })();
            match result {
                Ok(()) => Ok(()),
                Err(error) => {
                    // Once rename consumed the unique temporary file, the destination may
                    // already have changed even if the final read/verification failed.
                    let mutated = !temporary.exists()
                        || (operation.kind == PlannedOperationKind::Create
                            && fs::read(&operation.path).map(|bytes| bytes == expected).unwrap_or(false));
                    if let Err(cleanup) = fs::remove_file(&temporary) {
                        if cleanup.kind() != std::io::ErrorKind::NotFound {
                            return Err((format!("{error}; не удалось удалить временный файл {}: {cleanup}", temporary.display()), mutated));
                        }
                    }
                    Err((error.to_string(), mutated))
                }
            }
        }
    }
}

fn set_owner(path: &Path, uid: u32, gid: u32) -> AppResult<()> {
    let output = Command::new("chown").arg(format!("{uid}:{gid}")).arg(path).output()
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
                if operation.path.exists() {
                    return Err(format!("Файл, запланированный к удалению, существует: {}", operation.path.display()).into());
                }
            }
            PlannedOperationKind::Create | PlannedOperationKind::Replace => {
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
                let expected = operation.content.as_deref().unwrap_or_default();
                retry_remove_created(&operation.path, expected, backup)
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
            PlannedOperationKind::Create => !operation.path.exists(),
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
    let _ = staged;
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
                Ok(()) if !path.exists() => return Ok(true),
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
    success: bool,
    stdout: String,
    stderr: String,
    timed_out: bool,
}

fn run_xkeen(args: &[&str], backup: &mut BackupSession) -> AppResult<CommandResult> {
    let command_text = format!("XKEEN_FOREGROUND=1 xkeen {}", args.join(" "));
    let started = timestamp();
    backup.event("COMMAND", &format!("Команда: {command_text}; начало: {started}."))?;
    let mut child = match Command::new("xkeen").args(args).env("XKEEN_FOREGROUND", "1")
        .stdout(Stdio::piped()).stderr(Stdio::piped()).spawn() {
        Ok(child) => child,
        Err(error) => {
            backup.event("COMMAND", &format!("Команда: {command_text}; ошибка запуска: {error}; завершение: {}.", timestamp()))?;
            return Err(format!("Не удалось запустить {command_text}: {error}").into());
        }
    };
    let stdout = child.stdout.take().ok_or("Не удалось получить stdout команды XKeen.")?;
    let stderr = child.stderr.take().ok_or("Не удалось получить stderr команды XKeen.")?;
    let out_thread = thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut reader = stdout;
        let _ = reader.read_to_end(&mut bytes);
        bytes
    });
    let err_thread = thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut reader = stderr;
        let _ = reader.read_to_end(&mut bytes);
        bytes
    });
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
    let stdout = String::from_utf8_lossy(&out_thread.join().unwrap_or_default()).into_owned();
    let stderr = String::from_utf8_lossy(&err_thread.join().unwrap_or_default()).into_owned();
    let code = status.as_ref().and_then(|value| value.code());
    let successful = status.as_ref().map(|value| value.success()).unwrap_or(false) && !timed_out;
    let ended = timestamp();
    backup.event("COMMAND", &format!(
        "Команда: {command_text}; завершение: {ended}; exit_code: {:?}; timeout: {}.\nstdout:\n{}\nstderr:\n{}",
        code, timed_out, if stdout.is_empty() { "<пусто>" } else { &stdout }, if stderr.is_empty() { "<пусто>" } else { &stderr }
    ))?;
    Ok(CommandResult { code, success: successful, stdout, stderr, timed_out })
}

fn restart_xkeen(backup: &mut BackupSession) -> AppResult<()> {
    let stop = run_xkeen(&["-stop"], backup)?;
    if !stop.success {
        return Err(format!("xkeen -stop завершился ошибкой (код {:?}, timeout={}).", stop.code, stop.timed_out).into());
    }
    let status = run_xkeen(&["-status"], backup)?;
    let stopped = is_stopped_status(status.code, &status.stdout);
    if !stopped {
        return Err(format!("Остановка XKeen не подтверждена: exit_code={:?}; stdout={}", status.code, status.stdout).into());
    }

    let start = run_xkeen(&["-start"], backup)?;
    if !start.success {
        return Err(format!("xkeen -start завершился ошибкой (код {:?}, timeout={}).", start.code, start.timed_out).into());
    }
    thread::sleep(Duration::from_secs(5));
    let status = run_xkeen(&["-status"], backup)?;
    if !is_running_status(status.code, &status.stdout) {
        return Err(format!("Запуск XKeen не подтверждён: exit_code={:?}; stdout={}", status.code, status.stdout).into());
    }
    backup.event("XKEEN", "Остановка, запуск и проверка статуса XKeen завершились успешно.")?;
    Ok(())
}

fn start_after_rollback(backup: &mut BackupSession) -> String {
    match run_xkeen(&["-start"], backup) {
        Ok(start) if start.success => {
            thread::sleep(Duration::from_secs(5));
            match run_xkeen(&["-status"], backup) {
                Ok(status) if is_running_status(status.code, &status.stdout) => {
                        backup.event("ROLLBACK", "После отката запуск XKeen подтверждён.").ok();
                        "запущен, статус подтверждён".to_owned()
                    }
                Ok(status) => {
                    let message = format!("После отката статус XKeen не подтверждён: code={:?}; stdout={}", status.code, status.stdout);
                    backup.event("ERROR", &message).ok();
                    message
                }
                Err(error) => {
                    backup.event("ERROR", &format!("Не удалось проверить XKeen после отката: {error}")).ok();
                    error.to_string()
                }
            }
        }
        Ok(start) => {
            let message = format!("Не удалось запустить XKeen после отката: code={:?}; timeout={}", start.code, start.timed_out);
            backup.event("ERROR", &message).ok();
            message
        }
        Err(error) => {
            backup.event("ERROR", &format!("Ошибка запуска XKeen после отката: {error}")).ok();
            error.to_string()
        }
    }
}

fn is_stopped_status(code: Option<i32>, stdout: &str) -> bool {
    code == Some(1) && stdout.contains("Прокси-клиент не запущен")
}

fn is_running_status(code: Option<i32>, stdout: &str) -> bool {
    code == Some(0)
        && stdout.contains("Прокси-клиент")
        && stdout.contains("запущен")
        && !stdout.contains("не запущен")
}

#[cfg(test)]
mod status_tests {
    use super::{is_running_status, is_stopped_status};

    #[test]
    fn accepts_only_explicit_stopped_status_with_expected_exit_code() {
        assert!(is_stopped_status(Some(1), "Прокси-клиент не запущен"));
        assert!(!is_stopped_status(Some(0), "Прокси-клиент не запущен"));
        assert!(!is_stopped_status(Some(1), "не удалось определить статус"));
    }

    #[test]
    fn accepts_only_explicit_running_status_with_zero_exit_code() {
        assert!(is_running_status(Some(0), "Прокси-клиент запущен"));
        assert!(!is_running_status(Some(1), "Прокси-клиент запущен"));
        assert!(!is_running_status(Some(0), "Прокси-клиент не запущен"));
        assert!(!is_running_status(Some(0), "не удалось определить статус"));
    }
}

fn timestamp() -> String {
    Command::new("date").arg("+%Y-%m-%d %H:%M:%S").output()
        .ok().filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .unwrap_or_else(|| SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs().to_string()).unwrap_or_else(|_| "unknown".to_owned()))
}
