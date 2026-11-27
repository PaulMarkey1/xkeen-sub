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
const TEST_URL: &str = "https://fsn1-speed.hetzner.com/100MB.bin";
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
    Success { speed_mbps: f64 },
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
        println!("Нет подключений для проверки пропускной способности.");
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
            return Err(format!(
                "Вспомогательный Xray не открыл тестовые порты за {} секунд.",
                HELPER_START_LIMIT.as_secs()
            ).into());
        }

        println!();
        println!("Проверка пропускной способности: {} подключений.", tags.len());
        println!("Этап 1: два прохода по всем узлам, 256 КБ, до 6 параллельных проверок.");
        println!("Этап 2: до 8 кандидатов, по два замера 512 КБ.");
        println!("Этап 3: до 3 финалистов, пять последовательных раундов по 512 КБ.");
        println!("Источник тестовых данных: файл Hetzner 100 MB (Range-запросы).");
        println!("Общий лимит — 60 секунд, включая запуск вспомогательного Xray.");

        let mut evidence: BTreeMap<String, Evidence> = tags.iter()
            .cloned().map(|tag| (tag, Evidence::default())).collect();
        let mut stage1_best: BTreeMap<String, f64> = BTreeMap::new();
        let mut stage1_403 = 0usize;
        let mut stage1_total = 0usize;

        // Stage 1: two independent passes; rotate the starting candidate on pass two.
        for pass in 0..2 {
            if Instant::now() >= deadline { break; }
            println!();
            println!("ЭТАП 1 — проход {}/2.", pass + 1);
            let mut order: Vec<usize> = (0..tags.len()).collect();
            if pass == 1 && !order.is_empty() {
                let shift = tags.len() / 2;
                order.rotate_left(shift);
            }
            for batch in order.chunks(CONCURRENCY) {
                if Instant::now() >= deadline { break; }
                let batch_tags: Vec<String> = batch.iter().map(|&i| tags[i].clone()).collect();
                let batch_ports: Vec<u16> = batch.iter().map(|&i| ports[i]).collect();
                let timeout = STAGE1_TIMEOUT.min(remaining(deadline));
                if timeout.is_zero() { break; }
                let results = run_parallel_batch(&batch_tags, &batch_ports, 256_000, timeout);
                for (tag, result) in batch_tags.iter().zip(results.into_iter()) {
                    stage1_total += 1;
                    let item = evidence.get_mut(tag).expect("evidence exists for each tag");
                    item.attempts += 1;
                    match result {
                        ProbeResult::Success { speed_mbps } => {
                            item.successes += 1;
                            stage1_best.entry(tag.clone())
                                .and_modify(|value| *value = value.max(speed_mbps))
                                .or_insert(speed_mbps);
                            println!("  {tag} — {:.2} Мбит/с", speed_mbps);
                        }
                        ProbeResult::Failure { reason, http_code } => {
                            if http_code == 403 { stage1_403 += 1; }
                            println!("  {tag} — {reason}");
                            if http_code == 429 {
                                finish_child(&mut child);
                                return Err("Тестовый сервер вернул HTTP 429; замеры остановлены, чтобы не усиливать ограничение.".into());
                            }
                        }
                    }
                }
            }
        }

        // A broad 403 response is a broken test source, not evidence that nodes are bad.
        if stage1_total > 0 && stage1_403 * 2 >= stage1_total {
            finish_child(&mut child);
            return Err(format!(
                "Источник проверки вернул HTTP 403 для {stage1_403} из {stage1_total} попыток. Результат признан недостоверным; текущие селекторы останутся без изменений."
            ).into());
        }

        let mut shortlist: Vec<(String, f64)> = stage1_best.into_iter().collect();
        shortlist.sort_by(|a, b| b.1.total_cmp(&a.1));
        shortlist.truncate(SHORTLIST_SIZE);
        if shortlist.is_empty() {
            finish_child(&mut child);
            return Err("На первом этапе нет ни одного достоверного замера; текущие селекторы сохранены.".into());
        }
        println!("После этапа 1 в коротком списке: {}.", shortlist.len());

        // Stage 2: two comparable 512 KB samples per shortlisted node.
        println!();
        println!("ЭТАП 2 — повторная проверка короткого списка.");
        let mut stage2_rank: Vec<(String, f64)> = Vec::new();
        for (tag, _) in &shortlist {
            if Instant::now() >= deadline { break; }
            let port = ports[tags.iter().position(|candidate| candidate == tag)
                .ok_or("Потерян индекс подключения при формировании короткого списка.")?];
            let mut samples = Vec::new();
            for attempt in 0..2 {
                if Instant::now() >= deadline { break; }
                let timeout = STAGE2_TIMEOUT.min(remaining(deadline));
                if timeout.is_zero() { break; }
                let item = evidence.get_mut(tag).expect("evidence exists");
                item.attempts += 1;
                match measure_candidate(port, 512_000, timeout) {
                    ProbeResult::Success { speed_mbps } => {
                        item.successes += 1;
                        item.comparable_speeds.push(speed_mbps);
                        samples.push(speed_mbps);
                        println!("  {tag}, замер {}/2 — {:.2} Мбит/с", attempt + 1, speed_mbps);
                    }
                    ProbeResult::Failure { reason, http_code } => {
                        println!("  {tag}, замер {}/2 — {reason}", attempt + 1);
                        if http_code == 429 {
                            finish_child(&mut child);
                            return Err("Тестовый сервер вернул HTTP 429; проверка остановлена.".into());
                        }
                    }
                }
            }
            if let Some(value) = median(&samples) {
                stage2_rank.push((tag.clone(), value));
            }
        }
        stage2_rank.sort_by(|a, b| b.1.total_cmp(&a.1));
        stage2_rank.truncate(FINALISTS_SIZE);
        if stage2_rank.is_empty() {
            finish_child(&mut child);
            return Err("На втором этапе не осталось кандидатов с повторными успешными замерами; селекторы сохранены.".into());
        }

        let finalist_tags: Vec<String> = stage2_rank.iter().map(|item| item.0.clone()).collect();
        println!();
        println!("ФИНАЛИСТЫ: {}", finalist_tags.join(", "));

        // Stage 3: five sequential rounds, rotating the first candidate each round.
        for round in 0..STAGE3_ROUNDS {
            if Instant::now() >= deadline { break; }
            println!();
            println!("ЭТАП 3 — раунд {}/{}.", round + 1, STAGE3_ROUNDS);
            for offset in 0..finalist_tags.len() {
                if Instant::now() >= deadline { break; }
                let index = (round + offset) % finalist_tags.len();
                let tag = &finalist_tags[index];
                let port = ports[tags.iter().position(|candidate| candidate == tag)
                    .ok_or("Потерян индекс финалиста.")?];
                let timeout = STAGE3_TIMEOUT.min(remaining(deadline));
                if timeout.is_zero() { break; }
                let item = evidence.get_mut(tag).expect("evidence exists");
                item.attempts += 1;
                match measure_candidate(port, 512_000, timeout) {
                    ProbeResult::Success { speed_mbps } => {
                        item.successes += 1;
                        item.comparable_speeds.push(speed_mbps);
                        println!("  {tag} — {:.2} Мбит/с", speed_mbps);
                    }
                    ProbeResult::Failure { reason, http_code } => {
                        println!("  {tag} — {reason}");
                        if http_code == 429 {
                            finish_child(&mut child);
                            return Err("Cloudflare вернул HTTP 429; проверка остановлена.".into());
                        }
                    }
                }
            }
        }

        let mut ranking: Vec<(String, f64)> = Vec::new();
        for tag in &finalist_tags {
            let item = evidence.get(tag).expect("evidence exists");
            let Some(med) = median(&item.comparable_speeds) else { continue };
            let deviations: Vec<f64> = item.comparable_speeds.iter()
                .map(|value| (value - med).abs()).collect();
            let mad = median(&deviations).unwrap_or(0.0);
            let robust_speed = (med - 1.4826 * mad).max(0.0);
            let reliability = (item.successes as f64 + 1.0) / (item.attempts as f64 + 2.0);
            let score = robust_speed * reliability;
            println!(
                "  {tag}: медиана {:.2}, MAD {:.2}, успешность {}/{}, устойчивый балл {:.2}",
                med, mad, item.successes, item.attempts, score
            );
            ranking.push((tag.clone(), score));
        }
        ranking.sort_by(|a, b| b.1.total_cmp(&a.1));
        finish_child(&mut child);

        if ranking.is_empty() {
            return Err("Не удалось собрать достаточные данные для выбора; прежние селекторы сохранены.".into());
        }
        if Instant::now() >= deadline {
            println!("Внимание: достигнут общий лимит времени; использованы только завершённые замеры.");
        }
        println!();
        println!("ИТОГОВЫЙ РЕЙТИНГ:");
        for (index, (tag, score)) in ranking.iter().enumerate() {
            println!("  {}. {} — {:.2}", index + 1, tag, score);
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
    ProbeResult::Success { speed_mbps: bytes as f64 * 8.0 / transfer / 1_000_000.0 }
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
