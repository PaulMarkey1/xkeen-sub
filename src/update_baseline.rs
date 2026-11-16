use std::error::Error;

use crate::backup::{BackupSession, BackupSource};
use crate::terminal::{info, success, title, ProgressBar};
use crate::update_state::RunState;
use crate::update_apply::{self, PlannedOperationKind};
use crate::update_merge::ArchivePlan;
use crate::update_plan::build_file_plan;
use crate::update_report::{print_baseline_plan, print_file_plan, print_subscription_summary};

type AppResult<T> = Result<T, Box<dyn Error>>;

/// Applies baseline-only changes without validating Xray or restarting XKeen.
pub(crate) fn apply_baseline_only(
    state: &RunState,
    backup: &mut BackupSession,
) -> AppResult<()> {
    print_subscription_summary(state);
    print_baseline_plan(state);

    if state.baseline_updates.is_empty() && state.baseline_removals.is_empty() {
        backup.event(
            "UPDATE",
            "Поддерживаемые подключения и baseline не требуют изменений. Конфигурации Xray не загружались.",
        )?;
        backup.set_outcome(
            "БЕЗ ИЗМЕНЕНИЙ",
            "Поддерживаемые подключения не изменились. Рабочие файлы и baseline не изменялись.",
            true,
        );
        println!();
        title("ОБНОВЛЕНИЕ ЗАВЕРШЕНО");
        success("Поддерживаемые подключения не изменились. Рабочие файлы и baseline оставлены без изменений.");
        return Ok(());
    }

    let archive = ArchivePlan { outbounds: Vec::new(), added: 0 };
    let plan = build_file_plan(&[], state, &archive)?;
    print_file_plan(&plan);
    backup.set_plan_count(plan.len());
    backup.event(
        "UPDATE",
        &format!(
            "Поддерживаемые подключения не изменились. Подготовлено операций только с baseline: {}. Проверка Xray и перезапуск XKeen не требуются.",
            plan.len()
        ),
    )?;

    let sources: Vec<BackupSource> = plan
        .iter()
        .filter(|operation| operation.kind != PlannedOperationKind::Create)
        .map(|operation| BackupSource {
            path: operation.path.clone(),
            category: operation.category.to_owned(),
        })
        .collect();
    let progress_total = if plan.is_empty() {
        0
    } else {
        sources.len() + plan.len() * 3 + 1
    };
    let mut progress = ProgressBar::new(progress_total);

    match backup.backup_files_with_progress(&sources, &mut progress) {
        Ok(saved) => {
            backup.event(
                "BACKUP",
                &format!("Резервирование baseline завершено: {saved} файлов сохранено и проверено."),
            )?;
            if let Err(error) = update_apply::execute_baseline_only(&plan, backup, &mut progress) {
                progress.stop("Обновление baseline остановлено");
                backup.set_outcome(
                    "ОШИБКА BASELINE",
                    &format!("Рабочие файлы Xray не затрагивались. Ошибка обновления baseline: {error}"),
                    false,
                );
                return Err(error);
            }
            progress.finish("Baseline обновлён");
            backup.set_outcome(
                "BASELINE ОБНОВЛЁН",
                "Обновлена только сохранённая версия подписки. Рабочие файлы не менялись, Xray не проверялся и XKeen не перезапускался.",
                true,
            );
        }
        Err(error) => {
            progress.stop("Резервирование baseline не удалось");
            backup.set_outcome(
                "ОШИБКА РЕЗЕРВИРОВАНИЯ",
                &format!("Обновление baseline не выполнено: {error}"),
                false,
            );
            return Err(error);
        }
    }

    println!();
    title("ОБНОВЛЕНИЕ ЗАВЕРШЕНО");
    success("Обновлён только baseline. Рабочие файлы не изменялись, перезапуск XKeen не выполнялся.");
    Ok(())
}
