use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::error::Error;
use std::io::{BufRead, BufReader, Read};
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
    const PING_TIMEOUT: Duration = Duration::from_secs(3);
    const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30);
    const GLOBAL_DEADLINE: Duration = Duration::from_secs(600);
    const DOWNLOAD_BYTES: u64 = 10_000_000;
    const DOWNLOAD_ROUNDS: usize = 3;

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
        println!("Алгоритм: пинг блоками по 5 → повторный пинг → половина лучших → 10 МБ × 3 в блоках по 3 → 3 финалиста.");
        println!("Финал: повторный пинг и три одновременных загрузки по 10 МБ.");
        println!("Итог: скорость 80% + пинг 20%. Рабочий Xray и рабочие конфигурации не изменяются.");

        // Stage 1: ping every outbound in batches of five, with a hard 3-second timeout.
        println!("\nЭТАП 1 — первичный пинг, блоки по 5, тайм-аут 3 секунды.");
        let mut first_ping: BTreeMap<String, f64> = BTreeMap::new();
        for (batch_index, batch) in ports.chunks(5).enumerate() {
            if remaining(deadline).is_zero() { break; }
            let batch_tags = tags[batch_index * 5..batch_index * 5 + batch.len()].to_vec();
            let timeout = PING_TIMEOUT.min(remaining(deadline));
            let results = run_parallel_ping_batch(&batch_tags, batch, timeout);
            for (tag, result) in batch_tags.iter().zip(results) {
                match result {
                    ProbeResult::Success { latency_ms, .. } => {
                        first_ping.insert(tag.clone(), latency_ms);
                        println!("  {tag} — {:.1} мс", latency_ms);
                    }
                    ProbeResult::Failure { reason, http_code } => {
                        println!("  {tag} — нет ответа: {reason}");
                        if http_code == 429 {
                            finish_child(&mut child);
                            return Err("Тестовый источник вернул HTTP 429; прежние селекторы сохранены.".into());
                        }
                    }
                }
            }
        }
        if first_ping.is_empty() {
            finish_child(&mut child);
            return Err("Ни одно подключение не ответило на первичный пинг; прежние селекторы сохранены.".into());
        }

        // Stage 2: retry only responders. The timeout equals the slowest successful
        // response time from stage 1, exactly as requested (no additional floor).
        let second_timeout_ms = first_ping.values().copied().fold(0.0_f64, f64::max);
        let second_timeout = Duration::from_secs_f64((second_timeout_ms / 1000.0).max(0.001));
        println!("\nЭТАП 2 — повторный пинг {} подключений; тайм-аут {:.1} мс.",
            first_ping.len(), second_timeout.as_secs_f64() * 1000.0);
        let mut responders: Vec<String> = first_ping.keys().cloned().collect();
        responders.sort();
        let mut second_ping: BTreeMap<String, f64> = BTreeMap::new();
        for (batch_index, batch_tags) in responders.chunks(5).enumerate() {
            if remaining(deadline).is_zero() { break; }
            let batch_ports: Vec<u16> = batch_tags.iter().map(|tag| {
                ports[tags.iter().position(|candidate| candidate == tag).unwrap()]
            }).collect();
            let results = run_parallel_ping_batch(batch_tags, &batch_ports, second_timeout.min(remaining(deadline)));
            for (tag, result) in batch_tags.iter().zip(results) {
                match result {
                    ProbeResult::Success { latency_ms, .. } => {
                        second_ping.insert(tag.clone(), latency_ms);
                        println!("  {tag} — {:.1} мс", latency_ms);
                    }
                    ProbeResult::Failure { reason, .. } => println!("  {tag} — исключён: {reason}"),
                }
            }
            let _ = batch_index;
        }
        if second_ping.is_empty() {
            finish_child(&mut child);
            return Err("Повторный пинг не прошёл ни для одного подключения; прежние селекторы сохранены.".into());
        }

        // Stage 3: keep the fastest half by second-stage ping; round odd counts up.
        let mut ping_rank: Vec<(String, f64)> = second_ping.iter()
            .map(|(tag, ping)| (tag.clone(), *ping)).collect();
        ping_rank.sort_by(|a, b| a.1.total_cmp(&b.1));
        let keep_count = (ping_rank.len() + 1) / 2;
        ping_rank.truncate(keep_count);
        let candidates: Vec<String> = ping_rank.iter().map(|item| item.0.clone()).collect();
        println!("\nЭТАП 3 — отбор половины по повторному пингу: {} из {}.",
            candidates.len(), second_ping.len());
        for (index, (tag, ping)) in ping_rank.iter().enumerate() {
            println!("  {}. {tag} — {:.1} мс", index + 1, ping);
        }

        // Stage 4: three 10 MB downloads per node, in batches of three.
        println!("\nЭТАП 4 — загрузка 10 МБ, блоки по 3, три раунда.");
        let mut stage_speed: BTreeMap<String, Vec<f64>> = candidates.iter()
            .cloned().map(|tag| (tag, Vec::new())).collect();
        for round in 0..DOWNLOAD_ROUNDS {
            println!("  Раунд {}/{}", round + 1, DOWNLOAD_ROUNDS);
            for batch_tags in candidates.chunks(3) {
                if remaining(deadline).is_zero() { break; }
                let batch_ports: Vec<u16> = batch_tags.iter().map(|tag| {
                    ports[tags.iter().position(|candidate| candidate == tag).unwrap()]
                }).collect();
                let timeout = DOWNLOAD_TIMEOUT.min(remaining(deadline));
                let results = run_parallel_batch(batch_tags, &batch_ports, DOWNLOAD_BYTES, timeout);
                for (tag, result) in batch_tags.iter().zip(results) {
                    match result {
                        ProbeResult::Success { speed_mbps, .. } => {
                            stage_speed.get_mut(tag).unwrap().push(speed_mbps);
                            println!("    {tag} — {:.2} Мбит/с", speed_mbps);
                        }
                        ProbeResult::Failure { reason, http_code } => {
                            println!("    {tag} — неудачный замер: {reason}");
                            if http_code == 429 {
                                finish_child(&mut child);
                                return Err("Тестовый источник вернул HTTP 429; прежние селекторы сохранены.".into());
                            }
                        }
                    }
                }
            }
        }

        let mut stage_rank: Vec<(String, f64)> = stage_speed.iter()
            .filter_map(|(tag, values)| median(values).map(|speed| (tag.clone(), speed)))
            .collect();
        stage_rank.sort_by(|a, b| b.1.total_cmp(&a.1));
        stage_rank.truncate(3);
        if stage_rank.is_empty() {
            finish_child(&mut child);
            return Err("Не удалось получить ни одного полного замера 10 МБ; прежние селекторы сохранены.".into());
        }
        let finalists: Vec<String> = stage_rank.iter().map(|item| item.0.clone()).collect();
        println!("\nЭТАП 5 — финалисты по медианной скорости: {}", finalists.join(", "));

        // Stage 6: re-ping finalists, then run three concurrent final download rounds.
        let finalist_ports: Vec<u16> = finalists.iter().map(|tag| {
            ports[tags.iter().position(|candidate| candidate == tag).unwrap()]
        }).collect();
        println!("\nЭТАП 6А — повторный пинг трёх финалистов.");
        let final_ping_results = run_parallel_ping_batch(&finalists, &finalist_ports, PING_TIMEOUT.min(remaining(deadline)));
        let mut final_ping: BTreeMap<String, f64> = BTreeMap::new();
        for (tag, result) in finalists.iter().zip(final_ping_results) {
            match result {
                ProbeResult::Success { latency_ms, .. } => {
                    final_ping.insert(tag.clone(), latency_ms);
                    println!("  {tag} — {:.1} мс", latency_ms);
                }
                ProbeResult::Failure { reason, .. } => println!("  {tag} — финальный пинг не пройден: {reason}"),
            }
        }

        println!("\nЭТАП 6Б — три одновременных финальных загрузки по 10 МБ.");
        let mut final_speed: BTreeMap<String, Vec<f64>> = finalists.iter()
            .cloned().map(|tag| (tag, Vec::new())).collect();
        for round in 0..DOWNLOAD_ROUNDS {
            println!("  Финальный раунд {}/{}", round + 1, DOWNLOAD_ROUNDS);
            if remaining(deadline).is_zero() { break; }
            let timeout = DOWNLOAD_TIMEOUT.min(remaining(deadline));
            let results = run_parallel_batch(&finalists, &finalist_ports, DOWNLOAD_BYTES, timeout);
            for (tag, result) in finalists.iter().zip(results) {
                match result {
                    ProbeResult::Success { speed_mbps, .. } => {
                        final_speed.get_mut(tag).unwrap().push(speed_mbps);
                        println!("    {tag} — {:.2} Мбит/с", speed_mbps);
                    }
                    ProbeResult::Failure { reason, http_code } => {
                        println!("    {tag} — неудачный замер: {reason}");
                        if http_code == 429 {
                            finish_child(&mut child);
                            return Err("Тестовый источник вернул HTTP 429 на финальном этапе; прежние селекторы сохранены.".into());
                        }
                    }
                }
            }
        }

        // Require all three final download samples and a successful final ping.
        // This avoids allowing one lucky transfer to decide the winner.
        let mut ranking: Vec<(String, f64)> = Vec::new();
        let mut metrics: BTreeMap<String, (f64, f64)> = BTreeMap::new();
        for tag in &finalists {
            let speeds = final_speed.get(tag).map(Vec::as_slice).unwrap_or(&[]);
            let Some(ping) = final_ping.get(tag).copied() else {
                println!("  {tag} — исключён: нет успешного финального пинга.");
                continue;
            };
            if speeds.len() != DOWNLOAD_ROUNDS {
                println!("  {tag} — исключён: получено {}/{} полных финальных загрузок.",
                    speeds.len(), DOWNLOAD_ROUNDS);
                continue;
            }
            let speed = median(speeds).unwrap_or(0.0);
            if speed > 0.0 && speed.is_finite() && ping > 0.0 && ping.is_finite() {
                metrics.insert(tag.clone(), (speed, ping));
            }
        }
        let max_speed = metrics.values().map(|value| value.0).fold(0.0_f64, f64::max);
        let min_ping = metrics.values().map(|value| value.1).fold(f64::INFINITY, f64::min);
        for (tag, (speed, ping)) in &metrics {
            let score = 100.0
                * (speed / max_speed).powf(0.8)
                * (min_ping / ping).powf(0.2);
            if score.is_finite() && score > 0.0 {
                println!("  {tag}: медиана {:.2} Мбит/с, медианный пинг {:.1} мс, балл {:.2}",
                    speed, ping, score);
                ranking.push((tag.clone(), score));
            }
        }
        ranking.sort_by(|a, b| b.1.total_cmp(&a.1));

        // Preserve the existing primary node when it is within the existing 5% hysteresis.
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
            return Err("Финальные измерения не дали трёх полных раундов и успешного пинга ни для одного финалиста; прежние селекторы сохранены.".into());
        }
        println!("\nИТОГОВЫЙ РЕЙТИНГ (80% скорость / 20% пинг):");
        for (index, (tag, score)) in ranking.iter().enumerate() {
            println!("  {}. {} — {:.2} балла", index + 1, tag, score);
        }
        Ok(ranking)
    })();

    let result = match result {
        Ok(ranking) => Ok(ranking),
        Err(error) => {
            match fs::read_to_string(&log_path) {
                Ok(log) if !log.trim().is_empty() => Err(
                    format!("{error}\nЖурнал вспомогательного Xray:\n{}", log.trim()).into()
                ),
                _ => Err(error),
            }
        }
    };
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


/// Measures repeated fixed-size Cloudflare downloads in one curl process. The first 300 ms
/// are connection/download warm-up; subsequent ranges must reuse the same
/// TCP/TLS connection. Sampling stops after two consecutive stable comparisons
/// (<=15% difference) and never exceeds the caller's hard time budget.
fn measure_adaptive_candidate(port: u16, timeout: Duration, expected_speed_mbps: f64) -> Result<(f64, usize, bool), String> {
    if timeout < Duration::from_millis(1600) {
        return Err("недостаточно времени для минимального окна замера 1,6 с".into());
    }
    let chunk_size: u64 = if expected_speed_mbps < 10.0 {
        256_000
    } else if expected_speed_mbps < 100.0 {
        1_000_000
    } else {
        4_000_000
    };
    const WARMUP: Duration = Duration::from_millis(300);
    const MIN_SAMPLE_TIME: Duration = Duration::from_millis(1600);
    const MAX_REQUESTS: usize = 256;

    let proxy = format!("http://127.0.0.1:{port}");
    let timeout_arg = format!("{:.3}", timeout.as_secs_f64());
    let connect_arg = format!("{:.3}", timeout.min(CURL_CONNECT_TIMEOUT).as_secs_f64());
    let url = download_url(chunk_size);
    let mut command = Command::new(CURL_BIN);
    command.args([
        "--silent", "--show-error", "--http1.1", "--no-buffer",
        "--proxy", &proxy,
        "--connect-timeout", &connect_arg,
        "--max-time", &timeout_arg,
        "--output", "/dev/null",
        "--write-out",
        "%{size_download}\\t%{time_starttransfer}\\t%{time_total}\\t%{http_code}\\t%{num_connects}\\n",
    ]);
    for _ in 0..MAX_REQUESTS {
        command.arg(&url);
    }
    let mut child = command.stdout(Stdio::piped()).stderr(Stdio::piped())
        .spawn().map_err(|error| format!("не удалось запустить curl: {error}"))?;
    let stdout = child.stdout.take().ok_or("не удалось открыть поток результатов curl")?;
    let stderr = child.stderr.take().ok_or("не удалось открыть поток ошибок curl")?;
    let stderr_reader = thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut output = String::new();
        let _ = reader.read_to_string(&mut output);
        output
    });
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
                if status != 200 || bytes != chunk_size {
                    failure = Some(format!("неполная загрузка Cloudflare: HTTP {status}, {bytes}/{chunk_size} байт"));
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
                    if started.elapsed() >= MIN_SAMPLE_TIME && stable_pairs >= 1 {
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
    let process_status = child.wait().ok();
    let _ = reader.join();
    let stderr = stderr_reader.join().unwrap_or_default();
    
    if let Some(reason) = failure {
        let process = process_status
            .map(|status| status.to_string())
            .unwrap_or_else(|| "статус процесса недоступен".to_owned());
        let diagnostic = if stderr.trim().is_empty() {
            String::new()
        } else {
            format!("; stderr curl: {}", stderr.trim())
        };
        return Err(format!("{reason}; curl: {process}{diagnostic}"));
    }
    if samples.is_empty() {
        let process = process_status
            .map(|status| status.to_string())
            .unwrap_or_else(|| "статус процесса недоступен".to_owned());
        let diagnostic = if stderr.trim().is_empty() {
            String::new()
        } else {
            format!("; stderr curl: {}", stderr.trim())
        };
        return Err(format!("не получено ни одного полного измерительного интервала после прогрева; curl: {process}{diagnostic}"));
    }
    let speed = median(&samples).ok_or("не удалось вычислить медиану скорости")?;
    Ok((speed, samples.len(), stable))
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
    use super::{curl_failure_reason, download_url, median, robust_speed_score};
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
