use serde_json::{json, Value};
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
const TEST_HOST: &str = "https://speed.cloudflare.com/__down?bytes=";
const WARMUP_BYTES: u64 = 1_000_000;
const TEST_SIZES: [u64; 3] = [2_000_000, 5_000_000, 10_000_000];
const TOP_COUNT: usize = 10;
const MAX_TIME: u64 = 3;

/// Selects ten candidates using a staged 2/5/10 MB throughput test.
/// The active Xray process and its configuration files are never modified.
pub(crate) fn test_outbounds_speed(
    documents: &[(String, Value)],
    tags: &[String],
) -> AppResult<()> {
    let candidates = eligible_tags(documents, tags);
    if candidates.is_empty() {
        println!("В рабочем 04_outbounds.json нет подключений для замера.");
        return Ok(());
    }
    if !Path::new(ASSET_DIR).is_dir() {
        return Err(format!("Каталог данных Xray не найден: {ASSET_DIR}").into());
    }

    println!();
    println!("Поэтапный сравнительный замер скорости подключений.");
    println!("Этап 1: все {} подключений — 2 МБ; затем топ-{} — 5 МБ и 10 МБ.", candidates.len(), TOP_COUNT);
    println!("Тайм-аут каждой загрузки: {} с. Перед каждым этапом для узла выполняется прогрев 1 МБ.", MAX_TIME);
    println!("В рейтинг попадают только полностью загруженные файлы с HTTP 200 и успешным завершением curl.");

    let mut finalists: Vec<CandidateScore> = Vec::new();

    println!();
    println!("ЭТАП 1/3 — проверка всех подключений (2 МБ)");
    for (index, tag) in candidates.iter().enumerate() {
        println!("[{}/{}] {}", index + 1, candidates.len(), tag);
        match measure_stage(documents, tag, TEST_SIZES[0]) {
            Ok(Some(speed)) => finalists.push(CandidateScore { tag: tag.clone(), rates: vec![speed] }),
            Ok(None) => {}
            Err(error) => println!("  Ошибка замера: {error}"),
        }
    }

    finalists.sort_by(|a, b| b.rates[0].total_cmp(&a.rates[0]));
    finalists.truncate(TOP_COUNT);

    if finalists.is_empty() {
        println!();
        println!("Не удалось получить ни одного корректного результата на этапе 2 МБ.");
        return Ok(());
    }

    println!();
    println!("В следующий этап проходят {} подключений:", finalists.len());
    for (index, candidate) in finalists.iter().enumerate() {
        println!("  {}. {} — {:.2} Мбит/с", index + 1, candidate.tag, candidate.rates[0]);
    }

    for (stage_index, size) in TEST_SIZES.iter().enumerate().skip(1) {
        println!();
        println!("ЭТАП {}/3 — проверка финалистов ({} МБ)", stage_index + 1, size / 1_000_000);
        for (index, candidate) in finalists.iter_mut().enumerate() {
            println!("[{}/{}] {}", index + 1, finalists.len(), candidate.tag);
            match measure_stage(documents, &candidate.tag, *size) {
                Ok(Some(speed)) => candidate.rates.push(speed),
                Ok(None) => {}
                Err(error) => println!("  Ошибка замера: {error}"),
            }
        }
    }

    let mut by_max: Vec<(&CandidateScore, f64)> = finalists.iter()
        .filter_map(|candidate| candidate.rates.iter().copied()
            .max_by(f64::total_cmp).map(|speed| (candidate, speed)))
        .collect();
    by_max.sort_by(|a, b| b.1.total_cmp(&a.1));

    let mut by_median: Vec<(&CandidateScore, f64)> = finalists.iter()
        .filter_map(|candidate| {
            let mut rates = candidate.rates.clone();
            rates.sort_by(f64::total_cmp);
            if rates.is_empty() { None } else { Some((candidate, median(&rates))) }
        })
        .collect();
    by_median.sort_by(|a, b| b.1.total_cmp(&a.1));

    println!();
    println!("ИТОГОВЫЙ РЕЙТИНГ — максимальная скорость среди замеров 2/5/10 МБ:");
    for (rank, (candidate, speed)) in by_max.iter().enumerate() {
        println!("  {}. {} — {:.2} Мбит/с", rank + 1, candidate.tag, speed);
    }

    println!();
    println!("ИТОГОВЫЙ РЕЙТИНГ — медиана доступных замеров 2/5/10 МБ:");
    for (rank, (candidate, speed)) in by_median.iter().enumerate() {
        println!("  {}. {} — {:.2} Мбит/с ({}/3 замеров)", rank + 1, candidate.tag, speed, candidate.rates.len());
    }

    Ok(())
}

#[derive(Debug)]
struct CandidateScore {
    tag: String,
    rates: Vec<f64>,
}

fn measure_stage(
    documents: &[(String, Value)],
    tag: &str,
    size: u64,
) -> AppResult<Option<f64>> {
    let samples = measure_one(documents, tag, &[size])?;
    let Some(sample) = samples.iter().find(|sample| sample.size == size) else {
        println!("  {:>2} МБ: результат curl не получен.", size / 1_000_000);
        return Ok(None);
    };

    if !sample.is_valid() {
        print_invalid_sample(sample);
        return Ok(None);
    }

    let speed = sample.bytes as f64 * 8.0 / sample.transfer_seconds / 1_000_000.0;
    println!("  {:>2} МБ: {:.2} Мбит/с ({:.2} МиБ/с)", size / 1_000_000, speed, speed / 8.388608);
    print_sample_diagnostics(sample);
    Ok(Some(speed))
}

fn median(sorted: &[f64]) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let middle = sorted.len() / 2;
    if sorted.len() % 2 == 0 {
        (sorted[middle - 1] + sorted[middle]) / 2.0
    } else {
        sorted[middle]
    }
}

#[derive(Debug)]
struct Sample {
    size: u64,
    bytes: u64,
    first_byte_seconds: f64,
    transfer_seconds: f64,
    total_seconds: f64,
    connect_seconds: f64,
    tls_seconds: f64,
    http_code: u16,
    curl_exit: i32,
    error: String,
    remote_ip: String,
    http_version: String,
    new_connections: u64,
}

impl Sample {
    fn is_valid(&self) -> bool {
        self.bytes == self.size
            && self.http_code == 200
            && self.curl_exit == 0
            && self.transfer_seconds > 0.0
    }
}

fn print_invalid_sample(sample: &Sample) {
    let mut reasons = Vec::new();
    if sample.bytes != sample.size {
        reasons.push(format!("получено {} из {} байт", sample.bytes, sample.size));
    }
    if sample.http_code != 200 {
        reasons.push(format!("HTTP {}", if sample.http_code == 0 { "000".to_owned() } else { sample.http_code.to_string() }));
    }
    if sample.curl_exit != 0 {
        reasons.push(format!("curl exit {}", sample.curl_exit));
    }
    if sample.transfer_seconds <= 0.0 {
        reasons.push("нет времени передачи".to_owned());
    }
    println!(
        "  {:>2} МБ: НЕКОРРЕКТНО — {}.",
        sample.size / 1_000_000,
        reasons.join(", ")
    );
    print_sample_diagnostics(sample);
    if !sample.error.is_empty() {
        println!("       Ошибка curl: {}", sample.error);
    }
}

fn print_sample_diagnostics(sample: &Sample) {
    println!(
        "       HTTP {}, curl {}, байты {}/{}, connect {:.3} с, TLS {:.3} с, первый байт {:.3} с, передача {:.3} с, всего {:.3} с",
        if sample.http_code == 0 { "000".to_owned() } else { sample.http_code.to_string() },
        sample.curl_exit,
        sample.bytes,
        sample.size,
        sample.connect_seconds,
        sample.tls_seconds,
        sample.first_byte_seconds,
        sample.transfer_seconds,
        sample.total_seconds
    );
    println!(
        "       IP {}, версия HTTP {}, новых соединений curl: {}",
        if sample.remote_ip.is_empty() { "—" } else { &sample.remote_ip },
        if sample.http_version.is_empty() { "—" } else { &sample.http_version },
        sample.new_connections
    );
}

fn median(sorted: &[f64]) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let middle = sorted.len() / 2;
    if sorted.len() % 2 == 0 {
        (sorted[middle - 1] + sorted[middle]) / 2.0
    } else {
        sorted[middle]
    }
}

fn eligible_tags(documents: &[(String, Value)], requested: &[String]) -> Vec<String> {
    let mut result = Vec::new();
    for tag in requested {
        let mut eligible = false;
        for (_, document) in documents {
            if let Some(outbounds) = document.get("outbounds").and_then(Value::as_array) {
                for outbound in outbounds {
                    if outbound.get("tag").and_then(Value::as_str) != Some(tag.as_str()) {
                        continue;
                    }
                    let protocol = outbound.get("protocol").and_then(Value::as_str).unwrap_or("");
                    if !matches!(protocol, "freedom" | "blackhole" | "dns" | "loopback") && tag != "api" {
                        eligible = true;
                    }
                }
            }
        }
        if eligible && !result.contains(tag) {
            result.push(tag.clone());
        }
    }
    result
}

fn measure_one(documents: &[(String, Value)], tag: &str, sizes: &[u64]) -> AppResult<Vec<Sample>> {
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let directory = PathBuf::from("/opt/tmp").join(format!("xkeen-sub-speed-{}-{nonce}", std::process::id()));
    fs::create_dir_all("/opt/tmp")?;
    fs::create_dir(&directory)?;
    let log_path = PathBuf::from("/tmp").join(format!("xkeen-sub-speed-{}-{nonce}.log", std::process::id()));

    let result = (|| -> AppResult<Vec<Sample>> {
        let port = reserve_port()?;
        let mut helper_documents = documents.to_vec();
        add_probe_config(&mut helper_documents, tag, port)?;

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
        let mut child = Command::new(&xray_bin)
            .arg("run")
            .env("XRAY_LOCATION_CONFDIR", &directory)
            .env("XRAY_LOCATION_ASSET", ASSET_DIR)
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(stderr))
            .spawn()
            .map_err(|e| format!("Не удалось запустить вспомогательный Xray: {e}"))?;

        let ready = wait_for_port(port, &mut child, Duration::from_secs(15));
        let samples_result = if ready {
            download_series(port, sizes)
        } else {
            let log_text = fs::read_to_string(&log_path).unwrap_or_default();
            let details = log_text.lines().rev().take(12).collect::<Vec<_>>()
                .into_iter().rev().collect::<Vec<_>>().join("\n");
            let message = if details.is_empty() {
                "HTTP probe inbound не запустился за 15 секунд".to_owned()
            } else {
                format!("HTTP probe inbound не запустился за 15 секунд. Лог Xray:\n{details}")
            };
            Err(message.into())
        };

        let _ = child.kill();
        let _ = child.wait();
        samples_result
    })();

    if let Err(error) = fs::remove_dir_all(&directory) {
        eprintln!("Не удалось удалить временный каталог {}: {error}", directory.display());
    }
    let _ = fs::remove_file(&log_path);
    result
}

fn add_probe_config(documents: &mut Vec<(String, Value)>, tag: &str, port: u16) -> AppResult<()> {
    let routing_index = documents.iter().position(|(_, doc)| doc.get("routing").is_some())
        .ok_or("В рабочей конфигурации не найден объект routing.")?;
    let routing = documents[routing_index].1.get_mut("routing")
        .and_then(Value::as_object_mut).ok_or("Объект routing должен быть JSON-объектом.")?;
    let rules = routing.entry("rules").or_insert_with(|| json!([]))
        .as_array_mut().ok_or("routing.rules должен быть массивом.")?;
    rules.insert(0, json!({
        "type": "field",
        "inboundTag": ["xkeen-sub-speed-probe"],
        "outboundTag": tag
    }));

    let inbound_index = documents.iter().position(|(_, doc)| doc.get("inbounds").is_some())
        .unwrap_or(routing_index);
    let root = documents[inbound_index].1.as_object_mut()
        .ok_or("Корень конфигурации должен быть JSON-объектом.")?;
    let inbounds = root.entry("inbounds").or_insert_with(|| json!([]))
        .as_array_mut().ok_or("inbounds должен быть массивом.")?;
    inbounds.push(json!({
        "tag": "xkeen-sub-speed-probe",
        "listen": "127.0.0.1",
        "port": port,
        "protocol": "http",
        "settings": {}
    }));
    Ok(())
}

fn download_series(port: u16, sizes: &[u64]) -> AppResult<Vec<Sample>> {
    let proxy = format!("http://127.0.0.1:{port}");
    let mut command = Command::new(CURL_BIN);
    command.args(["--silent", "--show-error", "--proxy", &proxy, "--max-time", &MAX_TIME.to_string()]);
    // A single curl process retains its connection cache between requests.
    command.args(["--output", "/dev/null", "--write-out", "WARMUP\n"]);
    command.arg(format!("{TEST_HOST}{WARMUP_BYTES}"));

    let mut expected = Vec::with_capacity(sizes.len());
    const WRITE_OUT: &str = "%{size_download}\t%{time_starttransfer}\t%{time_total}\t%{time_connect}\t%{time_appconnect}\t%{http_code}\t%{exitcode}\t%{errormsg}\t%{remote_ip}\t%{http_version}\t%{num_connects}\n";
    for size in sizes {
        command.args(["--output", "/dev/null", "--write-out", WRITE_OUT]);
        command.arg(format!("{TEST_HOST}{size}"));
        expected.push(*size);
    }

    let output = command.stdout(Stdio::piped()).stderr(Stdio::piped()).output()
        .map_err(|e| format!("Не удалось запустить curl: {e}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    let mut lines = stdout.lines();
    let _warmup = lines.next();
    let mut samples = Vec::with_capacity(expected.len());

    for size in expected {
        let line = lines.next().unwrap_or("");
        let fields: Vec<&str> = line.splitn(11, '\t').collect();
        let parse_u64 = |index: usize| fields.get(index).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
        let parse_f64 = |index: usize| fields.get(index).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
        let bytes = parse_u64(0);
        let first_byte_seconds = parse_f64(1);
        let total_seconds = parse_f64(2);
        let connect_seconds = parse_f64(3);
        let tls_seconds = parse_f64(4);
        let http_code = fields.get(5).and_then(|v| v.parse::<u16>().ok()).unwrap_or(0);
        let curl_exit = fields.get(6).and_then(|v| v.parse::<i32>().ok()).unwrap_or(-1);
        let error = fields.get(7).copied().unwrap_or("").to_owned();
        let remote_ip = fields.get(8).copied().unwrap_or("").to_owned();
        let http_version = fields.get(9).copied().unwrap_or("").to_owned();
        let new_connections = parse_u64(10);
        let transfer_seconds = (total_seconds - first_byte_seconds).max(0.0);

        let error = if error.is_empty() && curl_exit != 0 && !stderr.is_empty() {
            stderr.clone()
        } else {
            error
        };

        samples.push(Sample {
            size,
            bytes,
            first_byte_seconds,
            transfer_seconds,
            total_seconds,
            connect_seconds,
            tls_seconds: (tls_seconds - connect_seconds).max(0.0),
            http_code,
            curl_exit,
            error,
            remote_ip,
            http_version,
            new_connections,
        });
    }

    if output.status.code().is_some_and(|code| code != 0) && samples.iter().all(|sample| sample.curl_exit == -1) {
        return Err(format!(
            "curl завершился с кодом {} и не вернул данные. stderr: {}",
            output.status,
            if stderr.is_empty() { "пусто" } else { &stderr }
        ).into());
    }
    Ok(samples)
}

fn reserve_port() -> AppResult<u16> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

fn wait_for_port(port: u16, child: &mut Child, timeout: Duration) -> bool {
    let started = Instant::now();
    while started.elapsed() < timeout {
        if child.try_wait().ok().flatten().is_some() {
            return false;
        }
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return true;
        }
        thread::sleep(Duration::from_millis(250));
    }
    false
}

fn safe_config_name(name: &str) -> bool {
    !name.is_empty() && !name.contains('/') && !name.contains('\\') && name != "." && name != ".."
}
