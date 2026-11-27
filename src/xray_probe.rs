use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::error::Error;
use std::io::{BufRead, BufReader};
use std::sync::mpsc;
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

#[derive(Debug)]
enum ProbeResult {
    Success { speed_mbps: f64, latency_ms: f64 },
    Failure { reason: String, http_code: u16 },
}

/// Selects the best outbounds using repeated real throughput measurements.
/// The helper Xray runs from a temporary config; active Xray and config files are untouched.
/// Returns the complete finalist ranking so the caller can apply selector hysteresis.
pub(crate) fn select_advanced_outbounds(
    documents: &[(String, Value)],
    tags: &[String],
) -> AppResult<Vec<(String, f64)>> {
    let started = Instant::now();
    let deadline = started + GLOBAL_DEADLINE;
    if tags.is_empty() {
        println!("Нет подключений для проверки.");
        return Ok(Vec::new());
    }
    if !Path::new(ASSET_DIR).is_dir() {
        return Err(format!("Каталог данных Xray не найден: {ASSET_DIR}").into());
    }

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
        println!("Алгоритм: KeepAlive-пинг → быстрый отсев → адаптивный последовательный замер → 3 финалиста.");
        println!("Общий лимит — 60 секунд. Рабочий Xray и рабочие конфигурации не изменяются.");
        println!("Точные замеры выполняются по одному подключению; соединение curl сохраняется между измерениями.");

        // Stage 1: XKeen-UI DoubleKeepAlive ping, at most five nodes at once.
        println!("\nЭТАП 1 — KeepAlive-пинг всех подключений, группы до 5.");
        let mut initial_ping: BTreeMap<String, f64> = BTreeMap::new();
        for (batch_index, batch) in ports.chunks(5).enumerate() {
            if remaining(deadline).is_zero() { break; }
            let batch_tags = tags[batch_index * 5..batch_index * 5 + batch.len()].to_vec();
            let timeout = STAGE1_TIMEOUT.min(remaining(deadline));
            let results = run_parallel_ping_batch(&batch_tags, batch, timeout);
            for (tag, result) in batch_tags.iter().zip(results) {
                match result {
                    ProbeResult::Success { latency_ms, .. } => {
                        initial_ping.insert(tag.clone(), latency_ms);
                        println!("  {tag} — {:.1} мс (KeepAlive)", latency_ms);
                    }
                    ProbeResult::Failure { reason, http_code } => {
                        println!("  {tag} — пинг не измерен: {reason}");
                        if http_code == 429 {
                            finish_child(&mut child);
                            return Err("Тестовый источник вернул HTTP 429. Прежние селекторы сохранены.".into());
                        }
                    }
                }
            }
        }
        if initial_ping.is_empty() {
            finish_child(&mut child);
            return Err("Не удалось измерить KeepAlive-пинг ни одного подключения; прежние селекторы сохранены.".into());
        }

        // Stage 2: coarse speed screen in groups of five. Ping is diagnostic only;
        // it does not outweigh actual throughput in the final ranking.
        println!("\nЭТАП 2 — быстрый отсев всех подключений (256 КБ, группы до 5).");
        let mut quick_speed: BTreeMap<String, f64> = BTreeMap::new();
        for (batch_index, batch) in ports.chunks(5).enumerate() {
            if remaining(deadline).is_zero() { break; }
            let batch_tags = tags[batch_index * 5..batch_index * 5 + batch.len()].to_vec();
            let timeout = STAGE1_TIMEOUT.min(remaining(deadline));
            let results = run_parallel_batch(&batch_tags, batch, 256_000, timeout);
            for (tag, result) in batch_tags.iter().zip(results) {
                match result {
                    ProbeResult::Success { speed_mbps, .. } => {
                        quick_speed.insert(tag.clone(), speed_mbps);
                        println!("  {tag} — {:.2} Мбит/с", speed_mbps);
                    }
                    ProbeResult::Failure { reason, http_code } => {
                        println!("  {tag} — быстрый замер пропущен: {reason}");
                        if http_code == 429 {
                            finish_child(&mut child);
                            return Err("Тестовый источник вернул HTTP 429 на быстром отсеве. Прежние селекторы сохранены.".into());
                        }
                    }
                }
            }
        }
        if quick_speed.is_empty() {
            finish_child(&mut child);
            return Err("Быстрый отсев не дал успешных измерений; прежние селекторы сохранены.".into());
        }

        let mut quick_rank: Vec<(String, f64)> = quick_speed.iter()
            .map(|(tag, speed)| (tag.clone(), *speed))
            .collect();
        quick_rank.sort_by(|a, b| b.1.total_cmp(&a.1));
        quick_rank.truncate(8);
        let shortlist: Vec<String> = quick_rank.iter().map(|item| item.0.clone()).collect();
        println!("В точный замер проходят {} подключений: {}.",
            shortlist.len(), shortlist.join(", "));

        // Stage 3: adaptive tests, strictly one node at a time.
        // Each candidate gets up to 3 seconds, but reserve enough time for two
        // sequential final rounds on the three finalists.
        println!("\nЭТАП 3 — адаптивный замер топ-8 (последовательно, до 3 секунд на узел).");
        let final_reserve = Duration::from_secs(18);
        let mut coarse_results: BTreeMap<String, f64> = BTreeMap::new();
        for (index, tag) in shortlist.iter().enumerate() {
            let left = shortlist.len().saturating_sub(index).max(1) as u128;
            let available = remaining(deadline).saturating_sub(final_reserve);
            let fair_share = Duration::from_millis((available.as_millis() / left) as u64);
            let timeout = Duration::from_secs(3).min(fair_share);
            if timeout < Duration::from_millis(650) {
                println!("  {tag} — пропущен: сохраняем время для финалистов.");
                continue;
            }
            let port = ports[tags.iter().position(|candidate| candidate == tag).unwrap()];
            match measure_adaptive_candidate(port, timeout) {
                Ok((speed, samples, stable)) => {
                    coarse_results.insert(tag.clone(), speed);
                    println!("  {tag} — {:.2} Мбит/с, {} интервалов{}",
                        speed, samples, if stable { ", скорость стабилизировалась" } else { ", без стабилизации" });
                }
                Err(reason) => {
                    println!("  {tag} — точный замер не прошёл: {reason}");
                    if reason.contains("429") {
                        finish_child(&mut child);
                        return Err("Тестовый источник ограничил запросы (HTTP 429). Прежние селекторы сохранены.".into());
                    }
                }
            }
        }
        if coarse_results.is_empty() {
            finish_child(&mut child);
            return Err("Не осталось подключений с успешным адаптивным замером; прежние селекторы сохранены.".into());
        }

        let mut finalists_rank: Vec<(String, f64)> = coarse_results.iter()
            .map(|(tag, speed)| (tag.clone(), *speed))
            .collect();
        finalists_rank.sort_by(|a, b| b.1.total_cmp(&a.1));
        finalists_rank.truncate(3);
        let finalists: Vec<String> = finalists_rank.iter().map(|item| item.0.clone()).collect();
        println!("\nФИНАЛИСТЫ: {}", finalists.join(", "));

        // Stage 4: two sequential final rounds, rotating the order to remove
        // a fixed first/last-candidate advantage.
        println!("\nЭТАП 4 — два последовательных контрольных раунда финалистов.");
        let mut final_samples: BTreeMap<String, Vec<f64>> = finalists.iter()
            .cloned().map(|tag| (tag, Vec::new())).collect();
        let total_checks = finalists.len().saturating_mul(2);
        let mut completed_checks = 0usize;
        for round in 0..2 {
            println!("  Раунд {}/2", round + 1);
            for offset in 0..finalists.len() {
                if remaining(deadline) < Duration::from_millis(650) { break; }
                let index = (offset + round) % finalists.len();
                let tag = &finalists[index];
                let checks_left = total_checks.saturating_sub(completed_checks).max(1) as u128;
                let fair_share = Duration::from_millis((remaining(deadline).as_millis() / checks_left) as u64);
                let timeout = Duration::from_secs(3).min(fair_share);
                if timeout < Duration::from_millis(650) { break; }
                let port = ports[tags.iter().position(|candidate| candidate == tag).unwrap()];
                match measure_adaptive_candidate(port, timeout) {
                    Ok((speed, samples, stable)) => {
                        final_samples.get_mut(tag).unwrap().push(speed);
                        println!("    {tag} — {:.2} Мбит/с ({} интервалов{})",
                            speed, samples, if stable { ", стабильно" } else { "" });
                    }
                    Err(reason) => {
                        println!("    {tag} — ошибка: {reason}");
                        if reason.contains("429") {
                            finish_child(&mut child);
                            return Err("Тестовый источник вернул HTTP 429 на финальном этапе. Прежние селекторы сохранены.".into());
                        }
                    }
                }
                completed_checks += 1;
            }
        }

        let mut ranking: Vec<(String, f64)> = Vec::new();
        for tag in &finalists {
            let values = final_samples.get(tag).map(Vec::as_slice).unwrap_or(&[]);
            if values.is_empty() { continue; }
            let Some(score) = robust_speed_score(values, 2) else { continue; };
            let med = median(values).unwrap_or(0.0);
            let mad = median(&values.iter().map(|value| (value - med).abs()).collect::<Vec<_>>())
                .unwrap_or(0.0);
            let attempts = 2usize;
            let successes = values.len().min(attempts);
            println!("  {tag}: медиана {:.2}, MAD {:.2}, успешность {}/{}, итог {:.2} Мбит/с",
                med, mad, successes, attempts, score);
            if score.is_finite() && score > 0.0 {
                ranking.push((tag.clone(), score));
            }
        }
        ranking.sort_by(|a, b| b.1.total_cmp(&a.1));

        // Five-percent hysteresis: keep the current primary outbound if it is
        // still within 5% of the new best candidate.
        if let Some(current) = crate::current_connection::active_primary_tag() {
            if let (Some(best), Some(current_score)) = (
                ranking.first().map(|item| item.1),
                ranking.iter().find(|item| item.0 == current).map(|item| item.1),
            ) {
                if current_score >= best * 0.95 {
                    if let Some(position) = ranking.iter().position(|item| item.0 == current) {
                        let item = ranking.remove(position);
                        ranking.insert(0, item);
                        println!("Гистерезис 5%: сохраняем текущее подключение {current}.");
                    }
                }
            }
        }

        finish_child(&mut child);
        if ranking.is_empty() {
            return Err("Финальные замеры не дали надёжного результата; прежние селекторы сохранены.".into());
        }
        println!("\nИТОГОВЫЙ РЕЙТИНГ (медиана − 0,5 × MAD, с поправкой на успешность):");
        for (index, (tag, score)) in ranking.iter().enumerate() {
            println!("  {}. {} — {:.2} Мбит/с", index + 1, tag, score);
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

fn run_parallel_ping_batch(
    tags: &[String],
    ports: &[u16],
    timeout: Duration,
) -> Vec<ProbeResult> {
    thread::scope(|scope| {
        let handles: Vec<_> = tags.iter().zip(ports.iter()).map(|(tag, port)| {
            scope.spawn(move || {
                let _ = tag;
                measure_ping_candidate(*port, timeout)
            })
        }).collect();
        handles.into_iter().map(|handle| handle.join().unwrap_or_else(|_| {
            ProbeResult::Failure { reason: "поток проверки пинга завершился аварийно".into(), http_code: 0 }
        })).collect()
    })
}

/// Two HEAD requests run inside one curl process. curl's connection cache is shared
/// between transfers, so the second request reuses the first request's TCP/TLS session.
/// num_connects == 0 on transfer two is required; otherwise this was not KeepAlive.
fn measure_ping_candidate(port: u16, timeout: Duration) -> ProbeResult {
    let proxy = format!("http://127.0.0.1:{port}");
    let timeout_arg = format!("{:.3}", timeout.as_secs_f64());
    let connect_timeout = timeout.min(Duration::from_millis(700));
    let connect_arg = format!("{:.3}", connect_timeout.as_secs_f64());
    let output = match Command::new(CURL_BIN)
        .args([
            "--silent", "--show-error", "--http1.1", "--head",
            "--proxy", &proxy,
            "--connect-timeout", &connect_arg, "--max-time", &timeout_arg,
            "--output", "/dev/null", "--write-out",
            "%{time_starttransfer}\\t%{http_code}\\t%{num_connects}\\n",
            TEST_URL, TEST_URL,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
    {
        Ok(output) => output,
        Err(error) => return ProbeResult::Failure {
            reason: format!("не удалось запустить curl для пинга: {error}"), http_code: 0
        },
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    let rows: Vec<Vec<&str>> = stdout.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.split('\t').map(str::trim).collect())
        .collect();
    if rows.len() != 2 || rows.iter().any(|row| row.len() != 3) {
        return ProbeResult::Failure {
            reason: if stderr.is_empty() {
                format!("curl не вернул два результата KeepAlive: {}", stdout.trim())
            } else { stderr },
            http_code: 0,
        };
    }
    let first_status = rows[0][1].parse::<u16>().unwrap_or(0);
    let second_status = rows[1][1].parse::<u16>().unwrap_or(0);
    if first_status == 429 || second_status == 429 {
        return ProbeResult::Failure {
            reason: "HTTP 429 (ограничение тестового источника)".into(), http_code: 429
        };
    }
    if !(200..400).contains(&first_status) || !(200..400).contains(&second_status) {
        let status = if !(200..400).contains(&second_status) { second_status } else { first_status };
        return ProbeResult::Failure {
            reason: format!("HTTP {status} при HEAD-проверке"), http_code: status
        };
    }
    if !output.status.success() {
        return ProbeResult::Failure {
            reason: format!("ошибка curl при пинге: {}", output.status), http_code: second_status
        };
    }
    let reused_connections = rows[1][2].parse::<u32>().unwrap_or(u32::MAX);
    if reused_connections != 0 {
        return ProbeResult::Failure {
            reason: "сервер или прокси не сохранил KeepAlive-соединение".into(),
            http_code: second_status,
        };
    }
    let latency = rows[1][0].parse::<f64>().unwrap_or(f64::NAN) * 1000.0;
    if !latency.is_finite() || latency <= 0.0 {
        return ProbeResult::Failure {
            reason: "некорректное время ответа KeepAlive".into(), http_code: second_status
        };
    }
    ProbeResult::Success { speed_mbps: 0.0, latency_ms: latency }
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


/// Measures repeated fixed-size ranges in one curl process. The first 300 ms
/// are connection/download warm-up; subsequent ranges must reuse the same
/// TCP/TLS connection. Sampling stops after two consecutive stable comparisons
/// (<=15% difference) and never exceeds the caller's hard time budget.
fn measure_adaptive_candidate(port: u16, timeout: Duration) -> Result<(f64, usize, bool), String> {
    const CHUNK_SIZE: u64 = 1_000_000;
    const WARMUP: Duration = Duration::from_millis(300);
    const MIN_SAMPLE_TIME: Duration = Duration::from_millis(1600);
    const MAX_REQUESTS: usize = 256;

    let proxy = format!("http://127.0.0.1:{port}");
    let timeout_arg = format!("{:.3}", timeout.as_secs_f64());
    let connect_arg = format!("{:.3}", timeout.min(Duration::from_millis(900)).as_secs_f64());
    let range_arg = format!("0-{}", CHUNK_SIZE - 1);
    let mut command = Command::new(CURL_BIN);
    command.args([
        "--silent", "--show-error", "--http1.1", "--no-buffer",
        "--proxy", &proxy,
        "--connect-timeout", &connect_arg,
        "--max-time", &timeout_arg,
        "--range", &range_arg,
        "--output", "/dev/null",
        "--write-out",
        "%{size_download}\\t%{time_starttransfer}\\t%{time_total}\\t%{http_code}\\t%{num_connects}\\n",
    ]);
    for _ in 0..MAX_REQUESTS {
        command.arg(TEST_URL);
    }
    let mut child = command.stdout(Stdio::piped()).stderr(Stdio::null())
        .spawn().map_err(|error| format!("не удалось запустить curl: {error}"))?;
    let stdout = child.stdout.take().ok_or("не удалось открыть поток результатов curl")?;
    let (sender, receiver) = mpsc::channel::<String>();
    let reader = thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => if sender.send(line).is_err() { break; },
            }
        }
    });

    let started = Instant::now();
    let mut samples = Vec::new();
    let mut previous: Option<f64> = None;
    let mut stable_pairs = 0usize;
    let mut stable = false;
    let mut first_transfer = true;
    let mut failure: Option<String> = None;

    while started.elapsed() < timeout {
        match receiver.recv_timeout(Duration::from_millis(25)) {
            Ok(line) => {
                let fields: Vec<&str> = line.trim().split('\t').collect();
                if fields.len() != 5 { continue; }
                let bytes = fields[0].parse::<u64>().unwrap_or(0);
                let first_byte = fields[1].parse::<f64>().unwrap_or(0.0);
                let total = fields[2].parse::<f64>().unwrap_or(0.0);
                let status = fields[3].parse::<u16>().unwrap_or(0);
                let connections = fields[4].parse::<u32>().unwrap_or(u32::MAX);

                if status == 429 {
                    failure = Some("HTTP 429: тестовый источник ограничил запросы".into());
                    break;
                }
                if status != 206 || bytes != CHUNK_SIZE {
                    failure = Some(format!("неполный Range-ответ: HTTP {status}, {bytes}/{CHUNK_SIZE} байт"));
                    break;
                }
                if !first_transfer && connections != 0 {
                    failure = Some("KeepAlive не сохранился между последовательными загрузками".into());
                    break;
                }
                let was_first_transfer = first_transfer;
                first_transfer = false;
                if was_first_transfer { continue; }

                let transfer = total - first_byte;
                if !transfer.is_finite() || transfer <= 0.0 { continue; }
                let speed = bytes as f64 * 8.0 / transfer / 1_000_000.0;
                if started.elapsed() < WARMUP { continue; }
                samples.push(speed);

                if let Some(last) = previous {
                    let relative_delta = (speed - last).abs() / last.max(speed).max(0.001);
                    if relative_delta <= 0.15 {
                        stable_pairs += 1;
                    } else {
                        stable_pairs = 0;
                    }
                    if started.elapsed() >= MIN_SAMPLE_TIME && stable_pairs >= 2 {
                        stable = true;
                        break;
                    }
                }
                previous = Some(speed);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    let _ = child.kill();
    let _ = child.wait();
    let _ = reader.join();

    if let Some(reason) = failure { return Err(reason); }
    if samples.is_empty() {
        return Err("не получено ни одного полного измерительного интервала после прогрева".into());
    }
    let speed = median(&samples).ok_or("не удалось вычислить медиану скорости")?;
    Ok((speed, samples.len(), stable))
}

fn combined_score(speed: f64, max_speed: f64, latency_ms: f64, best_ping_ms: f64) -> f64 {
    let speed_score = if max_speed.is_finite() && max_speed > 0.0 {
        (speed / max_speed).clamp(0.0, 1.0)
    } else { 0.0 };
    let ping_score = if latency_ms.is_finite() && latency_ms > 0.0
        && best_ping_ms.is_finite() && best_ping_ms > 0.0 {
        (best_ping_ms / latency_ms).clamp(0.0, 1.0)
    } else { 0.0 };
    speed_score * 70.0 + ping_score * 30.0
}

fn robust_speed_score(values: &[f64], attempts: usize) -> Option<f64> {
    let med = median(values)?;
    let deviations: Vec<f64> = values.iter().map(|value| (value - med).abs()).collect();
    let mad = median(&deviations).unwrap_or(0.0);
    let reliability = values.len().min(attempts) as f64 / attempts.max(1) as f64;
    let score = (med - 0.5 * mad).max(0.0) * reliability;
    (score.is_finite() && score > 0.0).then_some(score)
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
    use super::{median, robust_speed_score};

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

    #[test]
    fn robust_score_penalizes_spread_with_mad() {
        let steady = robust_speed_score(&[100.0, 102.0], 2).unwrap();
        let unstable = robust_speed_score(&[50.0, 150.0], 2).unwrap();
        assert!(steady > unstable);
    }

    #[test]
    fn robust_score_penalizes_failed_final_round() {
        let both_successful = robust_speed_score(&[100.0, 102.0], 2).unwrap();
        let one_successful = robust_speed_score(&[100.0], 2).unwrap();
        assert!(both_successful > one_successful);
    }

    #[test]
    fn robust_score_rejects_empty_or_zero_samples() {
        assert_eq!(robust_speed_score(&[], 2), None);
        assert_eq!(robust_speed_score(&[0.0], 2), None);
    }
}
