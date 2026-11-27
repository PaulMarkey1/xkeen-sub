use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::error::Error;
use std::fs;
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

type AppResult<T> = Result<T, Box<dyn Error>>;

const ASSET_DIR: &str = "/opt/etc/xray/dat";
const XRAY_DEFAULT: &str = "/opt/sbin/xray";
const CURL_BIN: &str = "/opt/bin/curl";
const TEST_URL: &str = "https://speedtest.tele2.net/100MB.zip";
const GLOBAL_DEADLINE: Duration = Duration::from_secs(60);
const HELPER_START_LIMIT: Duration = Duration::from_secs(8);
const STAGE1_TIMEOUT: Duration = Duration::from_millis(1200);
const STAGE2_TIMEOUT: Duration = Duration::from_millis(1100);
const STAGE3_TIMEOUT: Duration = Duration::from_millis(1100);
const CONCURRENCY: usize = 6;
const SHORTLIST_SIZE: usize = 8;
const FINALISTS_SIZE: usize = 3;
const STAGE3_ROUNDS: usize = 5;

#[derive(Debug)]
enum ProbeResult {
    Success { speed_mbps: f64, latency_ms: f64 },
    Failure { reason: String, http_code: u16 },
}

#[derive(Default)]
struct Evidence {
    successes: usize,
    attempts: usize,
    comparable_speeds: Vec<f64>,
}

/// Selects the best outbounds using repeated real throughput measurements.
/// The helper Xray runs from a temporary config; active Xray and config files are untouched.
/// Returns the complete finalist ranking so the caller can apply selector hysteresis.
pub(crate) fn select_advanced_outbounds(
    documents: &[(String, Value)],
    tags: &[String],
) -> AppResult<Vec<(String, f64)>> {
    let started = Instant::now();
    if tags.is_empty() {
        println!("Нет подключений для проверки.");
        return Ok(Vec::new());
    }
    if !Path::new(ASSET_DIR).is_dir() {
        return Err(format!("Каталог данных Xray не найден: {ASSET_DIR}").into());
    }
    let deadline = started + GLOBAL_DEADLINE;
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let directory = PathBuf::from("/opt/tmp").join(format!(
        "xkeen-sub-speed-{}-{nonce}", std::process::id()
    ));
    let log_path = PathBuf::from("/tmp").join(format!(
        "xkeen-sub-speed-{}-{nonce}.log", std::process::id()
    ));
    fs::create_dir_all("/opt/tmp")?;
    fs::create_dir(&directory)?;

    let result = (|| -> AppResult<Vec<(String, f64)>> {
        let ports = (0..tags.len()).map(|_| reserve_port())
            .collect::<AppResult<Vec<_>>>()?;
        let mut helper_documents = documents.to_vec();
        configure_probe_inbounds(&mut helper_documents, tags, &ports)?;
        for (name, value) in &helper_documents {
            if !safe_config_name(name) {
                return Err(format!("Недопустимое имя конфигурационного файла: {name}").into());
            }
            fs::write(directory.join(name), serde_json::to_vec_pretty(value)?)?;
        }

        let xray_bin = std::env::var_os("XRAY_BIN").map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(XRAY_DEFAULT));
        let log = fs::File::create(&log_path)?;
        let stderr = log.try_clone()?;
        let child = Command::new(&xray_bin)
            .arg("run")
            .env("XRAY_LOCATION_CONFDIR", &directory)
            .env("XRAY_LOCATION_ASSET", ASSET_DIR)
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(stderr))
            .spawn()
            .map_err(|error| format!("Не удалось запустить вспомогательный Xray: {error}"))?;
        let mut child = HelperProcess(child);
        if !wait_for_ports(&ports, &mut child, HELPER_START_LIMIT.min(remaining(deadline))) {
            finish_child(&mut child);
            return Err("Вспомогательный Xray не открыл тестовые порты вовремя.".into());
        }

        println!();
        println!("Новый отбор: прогрев → пинг → быстрый отсев → 10 МБ → 3 финалиста.");
        println!("Ограничение конкурентности: 5 на крупных пакетах; общий лимит 60 секунд.");
        println!("Источник: Tele2 Speedtest 100 MB, Range-запросы. Рабочие конфиги не меняются до результата.");

        // One small warm-up request per node; results deliberately do not enter the score.
        println!("\nЭТАП 0 — прогрев подключений (32 КБ, группы до 5).");
        for (batch_index, batch) in ports.chunks(5).enumerate() {
            if Instant::now() >= deadline { break; }
            let batch_tags = tags[batch_index * 5..batch_index * 5 + batch.len()].to_vec();
            let warm = run_parallel_batch(&batch_tags, batch, 32_000, STAGE1_TIMEOUT.min(remaining(deadline)));
            for (tag, result) in batch_tags.iter().zip(warm) {
                match result {
                    ProbeResult::Success { .. } => println!("  {tag} — прогрето"),
                    ProbeResult::Failure { reason, http_code } => {
                        println!("  {tag} — прогрев пропущен: {reason}");
                        if http_code == 429 {
                            finish_child(&mut child);
                            return Err("Тестовый источник ограничил запросы (HTTP 429) на прогреве. Текущие селекторы сохранены.".into());
                        }
                    }
                }
            }
            thread::sleep(Duration::from_millis(250));
        }

        // Initial latency: one tiny request per node, in restrained batches.
        println!("\nЭТАП 1 — первичный пинг всех подключений.");
        let mut initial_ping: BTreeMap<String, f64> = BTreeMap::new();
        for (batch_index, batch) in ports.chunks(5).enumerate() {
            if Instant::now() >= deadline { break; }
            let batch_tags = tags[batch_index * 5..batch_index * 5 + batch.len()].to_vec();
            let results = run_parallel_batch(&batch_tags, batch, 32_000, STAGE1_TIMEOUT.min(remaining(deadline)));
            for (tag, result) in batch_tags.iter().zip(results) {
                match result {
                    ProbeResult::Success { latency_ms, .. } => {
                        initial_ping.insert(tag.clone(), latency_ms);
                        println!("  {tag} — {:.1} мс", latency_ms);
                    }
                    ProbeResult::Failure { reason, http_code } => {
                        println!("  {tag} — пинг не измерен: {reason}");
                        if http_code == 429 {
                            finish_child(&mut child);
                            return Err("Тестовый источник ограничил запросы (HTTP 429) при пинге. Текущие селекторы сохранены.".into());
                        }
                    }
                }
            }
            thread::sleep(Duration::from_millis(250));
        }
        if initial_ping.is_empty() {
            finish_child(&mut child);
            return Err("Не удалось получить достоверный пинг ни одного подключения; прежние селекторы сохранены.".into());
        }

        // Fast throughput screen, in groups of five. Ranking combines latency and speed.
        println!("\nЭТАП 2 — быстрый отсев всех узлов (256 КБ, до 5 одновременно).");
        let mut quick_speed: BTreeMap<String, f64> = BTreeMap::new();
        let mut quick_attempts = 0usize;
        for (batch_index, batch) in ports.chunks(5).enumerate() {
            if Instant::now() >= deadline { break; }
            let batch_tags = tags[batch_index * 5..batch_index * 5 + batch.len()].to_vec();
            let results = run_parallel_batch(&batch_tags, batch, 256_000, STAGE1_TIMEOUT.min(remaining(deadline)));
            for (tag, result) in batch_tags.iter().zip(results) {
                quick_attempts += 1;
                match result {
                    ProbeResult::Success { speed_mbps, .. } => {
                        quick_speed.insert(tag.clone(), speed_mbps);
                        println!("  {tag} — {:.2} Мбит/с", speed_mbps);
                    }
                    ProbeResult::Failure { reason, http_code } => {
                        println!("  {tag} — {reason}");
                        if http_code == 429 {
                            finish_child(&mut child);
                            return Err("Тестовый источник вернул HTTP 429 на быстром отсеве. Текущие селекторы сохранены.".into());
                        }
                    }
                }
            }
            thread::sleep(Duration::from_millis(300));
        }
        if quick_speed.is_empty() {
            finish_child(&mut child);
            return Err("Быстрый отсев не дал успешных измерений; прежние селекторы сохранены.".into());
        }
        let max_quick = quick_speed.values().copied().fold(0.0_f64, f64::max);
        let min_ping = initial_ping.values().copied().fold(f64::INFINITY, f64::min);
        let mut quick_rank: Vec<(String, f64)> = quick_speed.iter().map(|(tag, speed)| {
            let speed_score = if max_quick > 0.0 { speed / max_quick } else { 0.0 };
            let latency = initial_ping.get(tag).copied().unwrap_or(min_ping * 3.0);
            let ping_score = if latency.is_finite() && latency > 0.0 {
                (min_ping / latency).clamp(0.0, 1.0)
            } else { 0.0 };
            (tag.clone(), speed_score * 70.0 + ping_score * 30.0)
        }).collect();
        quick_rank.sort_by(|a, b| b.1.total_cmp(&a.1));
        quick_rank.truncate(10);
        let shortlist: Vec<String> = quick_rank.iter().map(|item| item.0.clone()).collect();
        println!("В следующий этап проходят {} узлов: {}", shortlist.len(), shortlist.join(", "));

        // Stage 3: a single 10 MB transfer for each of the ten nodes, batches of five.
        println!("\nЭТАП 3 — по 10 МБ на узел, пакеты по 5 подключений.");
        let mut large_speed: BTreeMap<String, f64> = BTreeMap::new();
        for batch in shortlist.chunks(5) {
            if Instant::now() >= deadline { break; }
            let batch_ports: Vec<u16> = batch.iter().filter_map(|tag|
                tags.iter().position(|candidate| candidate == tag).map(|index| ports[index])
            ).collect();
            let results = run_parallel_batch(batch, &batch_ports, 10_000_000, Duration::from_secs(8).min(remaining(deadline)));
            for (tag, result) in batch.iter().zip(results) {
                match result {
                    ProbeResult::Success { speed_mbps, .. } => {
                        large_speed.insert(tag.clone(), speed_mbps);
                        println!("  {tag} — {:.2} Мбит/с (10 МБ)", speed_mbps);
                    }
                    ProbeResult::Failure { reason, http_code } => {
                        println!("  {tag} — замер 10 МБ не прошёл: {reason}");
                        if http_code == 429 {
                            finish_child(&mut child);
                            return Err("Тестовый источник вернул HTTP 429 на замере 10 МБ. Текущие селекторы сохранены.".into());
                        }
                    }
                }
            }
            thread::sleep(Duration::from_millis(400));
        }
        if large_speed.is_empty() {
            finish_child(&mut child);
            return Err("Не удалось измерить скорость на 10 МБ; прежние селекторы сохранены.".into());
        }

        let max_large = large_speed.values().copied().fold(0.0_f64, f64::max);
        let min_short_ping = shortlist.iter().filter_map(|tag| initial_ping.get(tag).copied())
            .fold(f64::INFINITY, f64::min);
        let mut finalist_rank: Vec<(String, f64)> = large_speed.iter().map(|(tag, speed)| {
            let speed_score = if max_large > 0.0 { speed / max_large } else { 0.0 };
            let latency = initial_ping.get(tag).copied().unwrap_or(min_short_ping * 3.0);
            let ping_score = if latency.is_finite() && latency > 0.0 {
                (min_short_ping / latency).clamp(0.0, 1.0)
            } else { 0.0 };
            (tag.clone(), speed_score * 70.0 + ping_score * 30.0)
        }).collect();
        finalist_rank.sort_by(|a, b| b.1.total_cmp(&a.1));
        finalist_rank.truncate(3);
        let finalists: Vec<String> = finalist_rank.iter().map(|item| item.0.clone()).collect();
        println!("Финалисты: {}", finalists.join(", "));
        if finalists.is_empty() {
            finish_child(&mut child);
            return Err("После замеров 10 МБ не осталось финалистов; прежние селекторы сохранены.".into());
        }

        // Three simultaneous 10 MB rounds, rotating the order to reduce systematic bias.
        println!("\nЭТАП 4 — три раунда по 10 МБ, одновременно 3 финалиста.");
        let mut final_samples: BTreeMap<String, Vec<f64>> = finalists.iter()
            .cloned().map(|tag| (tag, Vec::new())).collect();
        for round in 0..3 {
            if Instant::now() >= deadline { break; }
            println!("  Раунд {}/3", round + 1);
            let order: Vec<String> = (0..finalists.len())
                .map(|offset| finalists[(round + offset) % finalists.len()].clone()).collect();
            let batch_ports: Vec<u16> = order.iter().filter_map(|tag|
                tags.iter().position(|candidate| candidate == tag).map(|index| ports[index])
            ).collect();
            let results = run_parallel_batch(&order, &batch_ports, 10_000_000, Duration::from_secs(8).min(remaining(deadline)));
            for (tag, result) in order.iter().zip(results) {
                match result {
                    ProbeResult::Success { speed_mbps, .. } => {
                        final_samples.get_mut(tag).expect("sample vector exists").push(speed_mbps);
                        println!("    {tag} — {:.2} Мбит/с", speed_mbps);
                    }
                    ProbeResult::Failure { reason, http_code } => {
                        println!("    {tag} — {reason}");
                        if http_code == 429 {
                            finish_child(&mut child);
                            return Err("Тестовый источник вернул HTTP 429 на финальном этапе. Текущие селекторы сохранены.".into());
                        }
                    }
                }
            }
            thread::sleep(Duration::from_millis(350));
        }

        // Final latency check after sustained transfers; use a single tiny request per finalist.
        println!("\nЭТАП 5 — повторный пинг финалистов после нагрузки.");
        let ping_ports: Vec<u16> = finalists.iter().filter_map(|tag|
            tags.iter().position(|candidate| candidate == tag).map(|index| ports[index])
        ).collect();
        let ping_results = run_parallel_batch(&finalists, &ping_ports, 32_000, STAGE1_TIMEOUT.min(remaining(deadline)));
        let mut final_ping: BTreeMap<String, f64> = BTreeMap::new();
        for (tag, result) in finalists.iter().zip(ping_results) {
            match result {
                ProbeResult::Success { latency_ms, .. } => {
                    final_ping.insert(tag.clone(), latency_ms);
                    println!("  {tag} — {:.1} мс", latency_ms);
                }
                ProbeResult::Failure { reason, http_code } => {
                    println!("  {tag} — финальный пинг недоступен: {reason}");
                    if http_code == 429 {
                        finish_child(&mut child);
                        return Err("Тестовый источник ограничил финальный пинг (HTTP 429). Текущие селекторы сохранены.".into());
                    }
                }
            }
        }

        let mut speed_medians: BTreeMap<String, f64> = BTreeMap::new();
        for tag in &finalists {
            if let Some(value) = median(final_samples.get(tag).map(Vec::as_slice).unwrap_or(&[])) {
                speed_medians.insert(tag.clone(), value);
            }
        }
        if speed_medians.is_empty() {
            finish_child(&mut child);
            return Err("Финальные измерения скорости не завершились успешно; прежние селекторы сохранены.".into());
        }
        let max_speed = speed_medians.values().copied().fold(0.0_f64, f64::max);
        let best_ping = final_ping.values().copied().fold(f64::INFINITY, f64::min);
        let mut ranking: Vec<(String, f64)> = speed_medians.iter().map(|(tag, speed)| {
            let speed_score = if max_speed > 0.0 { speed / max_speed } else { 0.0 };
            let latency = final_ping.get(tag).copied().unwrap_or(best_ping * 2.0);
            let ping_score = if latency.is_finite() && latency > 0.0 && best_ping.is_finite() {
                (best_ping / latency).clamp(0.0, 1.0)
            } else { 0.0 };
            let score = speed_score * 70.0 + ping_score * 30.0;
            println!("  {tag}: медиана скорости {:.2} Мбит/с, пинг {:.1} мс, итог {:.1}/100",
                speed, latency, score);
            (tag.clone(), score)
        }).collect();
        ranking.sort_by(|a, b| b.1.total_cmp(&a.1));
        finish_child(&mut child);
        if ranking.is_empty() {
            return Err("Итоговый рейтинг пуст; прежние селекторы сохранены.".into());
        }
        if Instant::now() >= deadline {
            println!("Предупреждение: общий лимит 60 секунд достигнут; учтены только завершённые проверки.");
        }
        println!("\nИТОГОВЫЙ РЕЙТИНГ (скорость 70%, пинг 30%):");
        for (index, (tag, score)) in ranking.iter().enumerate() {
            println!("  {}. {} — {:.1}/100", index + 1, tag, score);
        }
        Ok(ranking)
    })();

    if let Err(error) = fs::remove_dir_all(&directory) {
        eprintln!("Не удалось удалить временный каталог {}: {error}", directory.display());
    }
    let _ = fs::remove_file(&log_path);
    println!("Общее время проверки: {:.2} с.", started.elapsed().as_secs_f64());
    result
}

fn configure_probe_inbounds(
    documents: &mut Vec<(String, Value)>,
    tags: &[String],
    ports: &[u16],
) -> AppResult<()> {
    let routing_index = documents.iter().position(|(_, doc)| doc.get("routing").is_some())
        .ok_or("В конфигурации Xray не найден объект routing.")?;
    {
        let routing = documents[routing_index].1.get_mut("routing")
            .and_then(Value::as_object_mut).ok_or("Объект routing должен быть JSON-объектом.")?;
        let rules = routing.entry("rules").or_insert_with(|| json!([]))
            .as_array_mut().ok_or("routing.rules должен быть массивом.")?;
        for (index, tag) in tags.iter().enumerate() {
            rules.insert(0, json!({
                "type": "field",
                "inboundTag": [format!("xkeen-sub-speed-probe-{index}")],
                "outboundTag": tag
            }));
        }
    }
    let inbound_index = documents.iter().position(|(_, doc)| doc.get("inbounds").is_some())
        .unwrap_or(routing_index);
    {
        let root = documents[inbound_index].1.as_object_mut()
            .ok_or("Корень конфигурации должен быть JSON-объектом.")?;
        let inbounds = root.entry("inbounds").or_insert_with(|| json!([]))
            .as_array_mut().ok_or("inbounds должен быть массивом.")?;
        for (index, port) in ports.iter().enumerate() {
            inbounds.push(json!({
                "tag": format!("xkeen-sub-speed-probe-{index}"),
                "listen": "127.0.0.1",
                "port": port,
                "protocol": "http",
                "settings": {}
            }));
        }
    }
    Ok(())
}

fn run_parallel_batch(
    tags: &[String],
    ports: &[u16],
    size: u64,
    timeout: Duration,
) -> Vec<ProbeResult> {
    thread::scope(|scope| {
        let handles: Vec<_> = tags.iter().zip(ports.iter()).map(|(tag, port)| {
            scope.spawn(move || {
                let _ = tag;
                measure_candidate(*port, size, timeout)
            })
        }).collect();
        handles.into_iter().map(|handle| handle.join().unwrap_or_else(|_| {
            ProbeResult::Failure { reason: "поток проверки завершился аварийно".into(), http_code: 0 }
        })).collect()
    })
}

fn measure_candidate(port: u16, size: u64, timeout: Duration) -> ProbeResult {
    let proxy = format!("http://127.0.0.1:{port}");
    let timeout_arg = format!("{:.3}", timeout.as_secs_f64());
    let connect_timeout = timeout.min(Duration::from_millis(700));
    let connect_arg = format!("{:.3}", connect_timeout.as_secs_f64());
    let range_arg = format!("0-{}", size.saturating_sub(1));
    let output = match Command::new(CURL_BIN)
        .args([
            "--silent", "--show-error", "--proxy", &proxy,
            "--connect-timeout", &connect_arg, "--max-time", &timeout_arg,
            "--range", &range_arg,
            "--output", "/dev/null", "--write-out",
            "%{size_download}\\t%{time_starttransfer}\\t%{time_total}\\t%{http_code}",
        ])
        .arg(TEST_URL)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output() {
            Ok(output) => output,
            Err(error) => return ProbeResult::Failure {
                reason: format!("не удалось запустить curl: {error}"), http_code: 0
            },
        };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    let fields: Vec<&str> = stdout.split('\t').map(str::trim).collect();
    if fields.len() != 4 {
        return ProbeResult::Failure {
            reason: if stderr.is_empty() { "curl не вернул статистику".into() } else { stderr },
            http_code: 0,
        };
    }
    let bytes = fields[0].parse::<u64>().unwrap_or(0);
    let first_byte = fields[1].parse::<f64>().unwrap_or(0.0);
    let total = fields[2].parse::<f64>().unwrap_or(0.0);
    let http_code = fields[3].parse::<u16>().unwrap_or(0);
    if http_code != 206 {
        return ProbeResult::Failure { reason: format!("HTTP {http_code} (ожидался 206 Partial Content)"), http_code };
    }
    if !output.status.success() {
        return ProbeResult::Failure {
            reason: format!("ошибка curl: {}", output.status),
            http_code,
        };
    }
    if bytes != size {
        return ProbeResult::Failure {
            reason: format!("неполная загрузка: {bytes}/{size} байт"),
            http_code,
        };
    }
    let transfer = total - first_byte;
    if !transfer.is_finite() || transfer <= 0.0 {
        return ProbeResult::Failure {
            reason: "некорректное время передачи данных".into(),
            http_code,
        };
    }
    ProbeResult::Success { speed_mbps: bytes as f64 * 8.0 / transfer / 1_000_000.0, latency_ms: first_byte * 1000.0 }
}

fn median(values: &[f64]) -> Option<f64> {
    if values.is_empty() { return None; }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let middle = sorted.len() / 2;
    Some(if sorted.len() % 2 == 0 {
        (sorted[middle - 1] + sorted[middle]) / 2.0
    } else {
        sorted[middle]
    })
}

fn remaining(deadline: Instant) -> Duration {
    deadline.checked_duration_since(Instant::now()).unwrap_or_default()
}

fn reserve_port() -> AppResult<u16> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

fn wait_for_ports(ports: &[u16], child: &mut HelperProcess, timeout: Duration) -> bool {
    let started = Instant::now();
    while started.elapsed() < timeout {
        if child.0.try_wait().ok().flatten().is_some() { return false; }
        if ports.iter().all(|port| TcpStream::connect(("127.0.0.1", *port)).is_ok()) {
            return true;
        }
        thread::sleep(Duration::from_millis(100));
    }
    false
}

fn finish_child(child: &mut HelperProcess) {
    let _ = child.0.kill();
    let _ = child.0.wait();
}

struct HelperProcess(Child);

impl Drop for HelperProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn safe_config_name(name: &str) -> bool {
    let path = Path::new(name);
    path.components().count() == 1
        && path.file_name().and_then(|value| value.to_str()) == Some(name)
        && matches!(path.extension().and_then(|value| value.to_str()), Some("json" | "jsonc"))
}

#[cfg(test)]
mod tests {
    use super::median;

    #[test]
    fn median_handles_odd_sample_count() {
        assert_eq!(median(&[30.0, 10.0, 20.0]), Some(20.0));
    }

    #[test]
    fn median_averages_even_sample_count() {
        assert_eq!(median(&[30.0, 10.0]), Some(20.0));
    }

    #[test]
    fn median_returns_none_for_empty_samples() {
        assert_eq!(median(&[]), None);
    }
}
