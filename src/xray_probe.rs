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
const TEST_URL: &str = "https://speed.cloudflare.com/__down";
const GLOBAL_DEADLINE: Duration = Duration::from_secs(60);
const HELPER_START_LIMIT: Duration = Duration::from_secs(8);
const STAGE1_TIMEOUT: Duration = Duration::from_millis(1600);
const CURL_CONNECT_TIMEOUT: Duration = Duration::from_millis(1100);

fn download_url(bytes: u64) -> String {
    format!("{TEST_URL}?bytes={bytes}")
}

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
    const PING_LIMIT: Duration = Duration::from_secs(3);
    const TRANSFER_LIMIT: Duration = Duration::from_secs(30);
    const TEST_BYTES: u64 = 10_000_000;
    const SAMPLES: usize = 3;
    const BATCH_SIZE: usize = 5;
    const TRANSFER_BATCH: usize = 3;
    const DEADLINE: Duration = Duration::from_secs(600);

    if tags.is_empty() {
        println!("Нет подключений для проверки.");
        return Ok(Vec::new());
    }
    if !Path::new(ASSET_DIR).is_dir() {
        return Err(format!("Каталог данных Xray не найден: {ASSET_DIR}").into());
    }

    let expires = Instant::now() + DEADLINE;
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let work_dir = PathBuf::from("/opt/tmp")
        .join(format!("xkeen-sub-probe-{}-{stamp}", std::process::id()));
    let log_file = PathBuf::from("/tmp")
        .join(format!("xkeen-sub-probe-{}-{stamp}.log", std::process::id()));
    fs::create_dir_all("/opt/tmp")?;
    fs::create_dir(&work_dir)?;

    let operation = (|| -> AppResult<Vec<(String, f64)>> {
        let ports = (0..tags.len()).map(|_| reserve_port())
            .collect::<AppResult<Vec<_>>>()?;
        let mut isolated_config = documents.to_vec();
        configure_probe_inbounds(&mut isolated_config, tags, &ports)?;
        for (filename, document) in &isolated_config {
            if !safe_config_name(filename) {
                return Err(format!("Недопустимое имя файла Xray: {filename}").into());
            }
            fs::write(work_dir.join(filename), serde_json::to_vec_pretty(document)?)?;
        }

        let binary = std::env::var_os("XRAY_BIN")
            .map(PathBuf::from).unwrap_or_else(|| PathBuf::from(XRAY_DEFAULT));
        let stdout = fs::File::create(&log_file)?;
        let stderr = stdout.try_clone()?;
        let process = Command::new(binary)
            .arg("run")
            .env("XRAY_LOCATION_CONFDIR", &work_dir)
            .env("XRAY_LOCATION_ASSET", ASSET_DIR)
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .spawn()
            .map_err(|e| format!("Не удалось запустить изолированный Xray: {e}"))?;
        let mut helper = HelperProcess(process);
        if !wait_for_ports(&ports, &mut helper, HELPER_START_LIMIT.min(remaining(expires))) {
            return Err("Изолированный Xray не открыл тестовые порты.".into());
        }

        println!("\nПроверка выполняется на временном Xray. Рабочие файлы и основной процесс не меняются.");
        println!("План: ping 5 → повторный ping → лучшая половина → замеры 3×10 МБ блоками по 3 → 3 финалиста → финальный ping и 3 параллельных замера.");
        let index_of = |tag: &str| tags.iter().position(|known| known == tag);

        // A. Initial reachability: batches of five, each with a three-second cap.
        println!("\nЭТАП 1. Первичный ping, блоки по {BATCH_SIZE}, лимит 3 с.");
        let mut alive = Vec::<(String, f64)>::new();
        for (offset, batch) in tags.chunks(BATCH_SIZE).enumerate() {
            if remaining(expires).is_zero() { break; }
            let batch_ports = &ports[offset * BATCH_SIZE..offset * BATCH_SIZE + batch.len()];
            let outcomes = run_parallel_ping_batch(batch, batch_ports, PING_LIMIT.min(remaining(expires)));
            for (tag, outcome) in batch.iter().zip(outcomes) {
                match outcome {
                    ProbeResult::Success { latency_ms, .. } => {
                        alive.push((tag.clone(), latency_ms));
                        println!("  {tag}: {latency_ms:.1} мс");
                    }
                    ProbeResult::Failure { reason, http_code } => {
                        println!("  {tag}: не отвечает ({reason})");
                        if http_code == 429 {
                            return Err("Источник тестирования вернул HTTP 429. Результат не применён.".into());
                        }
                    }
                }
            }
        }
        if alive.is_empty() {
            return Err("Первичный ping не прошёл ни для одного узла.".into());
        }

        // B. Retry only reachable nodes. Timeout is exactly the maximum measured latency.
        let retry_timeout = alive.iter().map(|(_, ms)| *ms).fold(0.0_f64, f64::max);
        let retry_timeout = Duration::from_secs_f64((retry_timeout / 1000.0).max(0.001));
        println!("\nЭТАП 2. Повторный ping {} узлов, тайм-аут {:.1} мс.",
            alive.len(), retry_timeout.as_secs_f64() * 1000.0);
        let alive_tags: Vec<String> = alive.iter().map(|(tag, _)| tag.clone()).collect();
        let mut confirmed = Vec::<(String, f64)>::new();
        for batch in alive_tags.chunks(BATCH_SIZE) {
            if remaining(expires).is_zero() { break; }
            let batch_ports: Vec<u16> = batch.iter().filter_map(|tag| index_of(tag).map(|i| ports[i])).collect();
            let outcomes = run_parallel_ping_batch(batch, &batch_ports, retry_timeout.min(remaining(expires)));
            for (tag, outcome) in batch.iter().zip(outcomes) {
                if let ProbeResult::Success { latency_ms, .. } = outcome {
                    confirmed.push((tag.clone(), latency_ms));
                    println!("  {tag}: {latency_ms:.1} мс — подтверждён");
                } else if let ProbeResult::Failure { reason, .. } = outcome {
                    println!("  {tag}: исключён ({reason})");
                }
            }
        }
        if confirmed.is_empty() {
            return Err("Повторный ping не подтвердил ни одного узла.".into());
        }

        // C. Keep the quickest half, rounding upward for an odd number.
        confirmed.sort_by(|a, b| a.1.total_cmp(&b.1));
        let retained = (confirmed.len() + 1) / 2;
        confirmed.truncate(retained);
        let screened: Vec<String> = confirmed.iter().map(|entry| entry.0.clone()).collect();
        println!("\nЭТАП 3. Оставляем {retained} из {} по повторному ping.", alive.len());
        for (rank, (tag, latency)) in confirmed.iter().enumerate() {
            println!("  {}. {tag}: {latency:.1} мс", rank + 1);
        }

        // D. Three fixed-size samples, grouped by three nodes. Each failed sample
        // receives exactly one retry; a node is eligible only with 3 valid samples.
        println!("\nЭТАП 4. Скрининг скорости: 3 загрузки по 10 МБ, блоки по 3 узла.");
        let mut screening: BTreeMap<String, Vec<f64>> =
            screened.iter().cloned().map(|tag| (tag, Vec::new())).collect();
        for group in screened.chunks(TRANSFER_BATCH) {
            println!("  Группа: {}", group.join(", "));
            let group_ports: Vec<u16> = group.iter().filter_map(|tag| index_of(tag).map(|i| ports[i])).collect();
            for sample_no in 1..=SAMPLES {
                if remaining(expires).is_zero() { break; }
                println!("    Измерение {sample_no}/{SAMPLES}");
                let outcomes = run_parallel_batch(group, &group_ports, TEST_BYTES, TRANSFER_LIMIT.min(remaining(expires)));
                for (tag, first) in group.iter().zip(outcomes) {
                    let result = match first {
                        ProbeResult::Success { speed_mbps, .. } => ProbeResult::Success { speed_mbps, latency_ms: 0.0 },
                        ProbeResult::Failure { reason, http_code } => {
                            println!("      {tag}: ошибка, один повтор ({reason})");
                            if http_code == 429 {
                                return Err("Источник тестирования вернул HTTP 429. Результат не применён.".into());
                            }
                            let port = ports[index_of(tag).ok_or("Не найден порт узла")?];
                            measure_candidate(port, TEST_BYTES, TRANSFER_LIMIT.min(remaining(expires)))
                        }
                    };
                    match result {
                        ProbeResult::Success { speed_mbps, .. } => {
                            screening.get_mut(tag).unwrap().push(speed_mbps);
                            println!("      {tag}: {speed_mbps:.2} Мбит/с");
                        }
                        ProbeResult::Failure { reason, http_code } => {
                            println!("      {tag}: повтор не прошёл ({reason})");
                            if http_code == 429 {
                                return Err("Источник тестирования вернул HTTP 429. Результат не применён.".into());
                            }
                        }
                    }
                }
            }
        }
        let mut speed_order: Vec<(String, f64)> = screening.iter()
            .filter(|(_, samples)| samples.len() == SAMPLES)
            .filter_map(|(tag, samples)| median(samples).map(|value| (tag.clone(), value)))
            .collect();
        speed_order.sort_by(|a, b| b.1.total_cmp(&a.1));
        speed_order.truncate(3);
        if speed_order.is_empty() {
            return Err("Нет узлов с тремя успешными скрининговыми измерениями.".into());
        }
        let finalists: Vec<String> = speed_order.iter().map(|entry| entry.0.clone()).collect();
        println!("\nЭТАП 5. Финалисты по медиане скорости: {}", finalists.join(", "));

        // E. Fresh ping for finalists.
        let finalist_ports: Vec<u16> = finalists.iter().filter_map(|tag| index_of(tag).map(|i| ports[i])).collect();
        println!("\nЭТАП 6. Финальный ping финалистов.");
        let ping_outcomes = run_parallel_ping_batch(&finalists, &finalist_ports, PING_LIMIT.min(remaining(expires)));
        let mut final_ping = BTreeMap::<String, f64>::new();
        for (tag, outcome) in finalists.iter().zip(ping_outcomes) {
            match outcome {
                ProbeResult::Success { latency_ms, .. } => {
                    final_ping.insert(tag.clone(), latency_ms);
                    println!("  {tag}: {latency_ms:.1} мс");
                }
                ProbeResult::Failure { reason, .. } => println!("  {tag}: исключён ({reason})"),
            }
        }

        // F. Three simultaneous final rounds. Failed transfers get one individual retry.
        println!("\nЭТАП 7. Три параллельных финальных загрузки по 10 МБ.");
        let mut final_samples: BTreeMap<String, Vec<f64>> =
            finalists.iter().cloned().map(|tag| (tag, Vec::new())).collect();
        for round in 1..=SAMPLES {
            if remaining(expires).is_zero() { break; }
            println!("  Раунд {round}/{SAMPLES}");
            let outcomes = run_parallel_batch(&finalists, &finalist_ports, TEST_BYTES, TRANSFER_LIMIT.min(remaining(expires)));
            for (tag, first) in finalists.iter().zip(outcomes) {
                let result = match first {
                    ProbeResult::Success { speed_mbps, .. } => ProbeResult::Success { speed_mbps, latency_ms: 0.0 },
                    ProbeResult::Failure { reason, http_code } => {
                        println!("    {tag}: ошибка, один повтор ({reason})");
                        if http_code == 429 {
                            return Err("Источник тестирования вернул HTTP 429 на финальном этапе.".into());
                        }
                        let port = ports[index_of(tag).ok_or("Не найден порт финалиста")?];
                        measure_candidate(port, TEST_BYTES, TRANSFER_LIMIT.min(remaining(expires)))
                    }
                };
                match result {
                    ProbeResult::Success { speed_mbps, .. } => {
                        final_samples.get_mut(tag).unwrap().push(speed_mbps);
                        println!("    {tag}: {speed_mbps:.2} Мбит/с");
                    }
                    ProbeResult::Failure { reason, http_code } => {
                        println!("    {tag}: повтор не прошёл ({reason})");
                        if http_code == 429 {
                            return Err("Источник тестирования вернул HTTP 429 на финальном этапе.".into());
                        }
                    }
                }
            }
        }

        // G. Score only complete candidates: 3/3 transfers and a valid final ping.
        let mut measured = BTreeMap::<String, (f64, f64)>::new();
        for tag in &finalists {
            let Some(ping) = final_ping.get(tag).copied() else {
                println!("  {tag}: исключён — финальный ping не прошёл");
                continue;
            };
            let samples = final_samples.get(tag).map(Vec::as_slice).unwrap_or(&[]);
            if samples.len() != SAMPLES {
                println!("  {tag}: исключён — финальных загрузок {}/{}", samples.len(), SAMPLES);
                continue;
            }
            if let Some(speed) = median(samples) {
                if speed.is_finite() && speed > 0.0 && ping.is_finite() && ping > 0.0 {
                    measured.insert(tag.clone(), (speed, ping));
                }
            }
        }
        let top_speed = measured.values().map(|v| v.0).fold(0.0_f64, f64::max);
        let best_ping = measured.values().map(|v| v.1).fold(f64::INFINITY, f64::min);
        let mut ranking: Vec<(String, f64)> = measured.iter().filter_map(|(tag, (speed, ping))| {
            let score = weighted_score(*speed, *ping, top_speed, best_ping)?;
            println!("  {tag}: медиана {:.2} Мбит/с, ping {:.1} мс, score {:.2}",
                speed, ping, score);
            Some((tag.clone(), score))
        }).collect();
        ranking.sort_by(|a, b| b.1.total_cmp(&a.1));

        // Existing five-percent hysteresis: retain the active primary if nearly tied.
        if let Some(active) = crate::current_connection::active_primary_tag() {
            if let (Some(best), Some(active_score)) = (
                ranking.first().map(|v| v.1),
                ranking.iter().find(|v| v.0 == active).map(|v| v.1),
            ) {
                if active_score >= best * 0.95 {
                    if let Some(position) = ranking.iter().position(|v| v.0 == active) {
                        let current = ranking.remove(position);
                        ranking.insert(0, current);
                        println!("Гистерезис 5%: оставляем активный узел {active}.");
                    }
                }
            }
        }
        if ranking.is_empty() {
            return Err("Ни один финалист не прошёл все финальные проверки. Текущие селекторы не затронуты.".into());
        }
        println!("\nРЕЙТИНГ (80% скорость / 20% ping):");
        for (position, (tag, score)) in ranking.iter().enumerate() {
            println!("  {}. {tag} — {score:.2}", position + 1);
        }
        finish_child(&mut helper);
        Ok(ranking)
    })();

    let result = match operation {
        Ok(value) => Ok(value),
        Err(error) => {
            let details = fs::read_to_string(&log_file).unwrap_or_default();
            if details.trim().is_empty() {
                Err(error)
            } else {
                Err(format!("{error}\nЖурнал временного Xray:\n{}", details.trim()).into())
            }
        }
    };
    let _ = fs::remove_dir_all(&work_dir);
    let _ = fs::remove_file(&log_file);
    result
}

fn weighted_score(speed: f64, ping: f64, maximum_speed: f64, minimum_ping: f64) -> Option<f64> {
    if !speed.is_finite() || !ping.is_finite()
        || !maximum_speed.is_finite() || !minimum_ping.is_finite()
        || speed <= 0.0 || ping <= 0.0 || maximum_speed <= 0.0 || minimum_ping <= 0.0
    {
        return None;
    }
    let score = 100.0 * (speed / maximum_speed).powf(0.8) * (minimum_ping / ping).powf(0.2);
    (score.is_finite() && score > 0.0).then_some(score)
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

/// Two zero-byte Cloudflare download requests run inside one curl process.
/// curl shares its connection cache between transfers; the second request must reuse
/// the first request's TCP/TLS session (num_connects == 0).
fn measure_ping_candidate(port: u16, timeout: Duration) -> ProbeResult {
    let proxy = format!("http://127.0.0.1:{port}");
    let timeout_arg = format!("{:.3}", timeout.as_secs_f64());
    let connect_timeout = timeout.min(CURL_CONNECT_TIMEOUT);
    let connect_arg = format!("{:.3}", connect_timeout.as_secs_f64());
    let ping_url = download_url(0);
    let output = match Command::new(CURL_BIN)
        .args([
            "--silent", "--show-error", "--http1.1",
            "--proxy", &proxy,
            "--connect-timeout", &connect_arg, "--max-time", &timeout_arg,
            "--output", "/dev/null", "--write-out",
            "%{time_starttransfer}\\t%{http_code}\\t%{num_connects}\\n",
            &ping_url, &ping_url,
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
    if !output.status.success() {
        return ProbeResult::Failure {
            reason: curl_failure_reason("KeepAlive GET", &output.status, &stderr, &stdout),
            http_code: 0,
        };
    }
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
    if first_status == 0 || second_status == 0 {
        return ProbeResult::Failure {
            reason: curl_failure_reason("KeepAlive GET (HTTP-ответ не получен)", &output.status, &stderr, &stdout),
            http_code: 0,
        };
    }
    if first_status == 429 || second_status == 429 {
        return ProbeResult::Failure {
            reason: "HTTP 429 (ограничение тестового источника)".into(), http_code: 429
        };
    }
    if !(200..400).contains(&first_status) || !(200..400).contains(&second_status) {
        let status = if !(200..400).contains(&second_status) { second_status } else { first_status };
        return ProbeResult::Failure {
            reason: format!("HTTP {status} при KeepAlive-проверке"), http_code: status
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

fn curl_failure_reason(
    stage: &str,
    status: &std::process::ExitStatus,
    stderr: &str,
    stdout: &str,
) -> String {
    let exit_code = status.code()
        .map(|code| code.to_string())
        .unwrap_or_else(|| status.to_string());
    let stderr = stderr.trim();
    let stdout = stdout.trim();
    let detail = if !stderr.is_empty() {
        stderr.to_owned()
    } else if !stdout.is_empty() {
        format!("вывод curl: {stdout}")
    } else {
        "curl не сообщил дополнительную причину".to_owned()
    };
    format!("{stage}: curl завершился с кодом {exit_code}; {detail}")
}

fn measure_candidate(port: u16, size: u64, timeout: Duration) -> ProbeResult {
    let proxy = format!("http://127.0.0.1:{port}");
    let timeout_arg = format!("{:.3}", timeout.as_secs_f64());
    let connect_timeout = timeout.min(CURL_CONNECT_TIMEOUT);
    let connect_arg = format!("{:.3}", connect_timeout.as_secs_f64());
    let url = download_url(size);
    let output = match Command::new(CURL_BIN)
        .args([
            "--silent", "--show-error", "--proxy", &proxy,
            "--connect-timeout", &connect_arg, "--max-time", &timeout_arg,
            "--output", "/dev/null", "--write-out",
            "%{size_download}\\t%{time_starttransfer}\\t%{time_total}\\t%{http_code}",
        ])
        .arg(&url)
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
    if !output.status.success() {
        return ProbeResult::Failure {
            reason: curl_failure_reason("Загрузка Cloudflare", &output.status, &stderr, &stdout),
            http_code: 0,
        };
    }
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
    if http_code == 0 {
        return ProbeResult::Failure {
            reason: curl_failure_reason("Загрузка Cloudflare (HTTP-ответ не получен)", &output.status, &stderr, &stdout),
            http_code: 0,
        };
    }
    if http_code != 200 {
        return ProbeResult::Failure { reason: format!("HTTP {http_code} (ожидался 200 OK)"), http_code };
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
    use super::{curl_failure_reason, download_url, median, weighted_score};
    use std::process::Command;

    #[test]
    fn cloudflare_download_url_uses_requested_byte_count() {
        assert_eq!(download_url(0), "https://speed.cloudflare.com/__down?bytes=0");
        assert_eq!(download_url(256_000), "https://speed.cloudflare.com/__down?bytes=256000");
    }

    #[test]
    fn curl_failure_diagnostic_preserves_exit_code_and_stderr() {
        let status = Command::new("sh").args(["-c", "exit 7"]).status().unwrap();
        let message = curl_failure_reason(
            "Загрузка Cloudflare",
            &status,
            "curl: (7) Failed to connect to 127.0.0.1",
            "",
        );
        assert!(message.contains("кодом 7"), "{message}");
        assert!(message.contains("Failed to connect"), "{message}");
    }

    #[test]
    fn curl_failure_diagnostic_falls_back_to_captured_stdout() {
        let status = Command::new("sh").args(["-c", "exit 28"]).status().unwrap();
        let message = curl_failure_reason("HEAD", &status, "  ", "diagnostic marker");
        assert!(message.contains("кодом 28"), "{message}");
        assert!(message.contains("diagnostic marker"), "{message}");
    }

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
    fn score_uses_eighty_twenty_weighting_and_normalizes_best_values() {
        assert!((weighted_score(100.0, 20.0, 100.0, 20.0).unwrap() - 100.0).abs() < 1e-9);
        let speed_only = weighted_score(50.0, 20.0, 100.0, 20.0).unwrap();
        let ping_only = weighted_score(100.0, 40.0, 100.0, 20.0).unwrap();
        assert!(speed_only > ping_only);
    }

    #[test]
    fn score_rejects_invalid_metrics() {
        assert_eq!(weighted_score(0.0, 10.0, 100.0, 10.0), None);
        assert_eq!(weighted_score(10.0, f64::NAN, 100.0, 10.0), None);
    }
}
