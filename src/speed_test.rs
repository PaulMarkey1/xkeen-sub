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
const TEST_SIZES: [u64; 4] = [2_000_000, 5_000_000, 10_000_000, 50_000_000];
const REPEATS: usize = 3;
const MAX_TIME: u64 = 8;

/// Measures download throughput after warming up one reusable HTTP/TLS connection.
/// Each candidate gets an isolated helper Xray; live Xray and configuration are untouched.
pub(crate) fn test_outbounds_speed(
    documents: &[(String, Value)],
    tags: &[String],
) -> AppResult<()> {
    let candidates = eligible_tags(documents, tags);
    if candidates.is_empty() {
        println!("Итоговая конфигурация не содержит подписочных подключений для замера.");
        return Ok(());
    }
    if !Path::new(ASSET_DIR).is_dir() {
        return Err(format!("Каталог данных Xray не найден: {ASSET_DIR}").into());
    }

    println!();
    println!("Сравнительный замер скорости подключений.");
    println!("Размеры: 2, 5, 10 и 50 МБ; повторов каждого размера: {REPEATS}.");
    println!("Перед замерами выполняется прогрев соединения; тайм-аут одной загрузки: {MAX_TIME} с.");
    println!("Запросы идут через один процесс curl, чтобы повторно использовать TCP/TLS-соединение.");

    let mut rankings: Vec<(String, f64)> = Vec::new();
    for (index, tag) in candidates.iter().enumerate() {
        println!();
        println!("[{}/{}] {}", index + 1, candidates.len(), tag);
        match measure_one(documents, tag) {
            Ok(samples) => {
                let mut all_rates = Vec::new();
                for size in TEST_SIZES {
                    let values: Vec<&Sample> = samples.iter()
                        .filter(|sample| sample.size == size && sample.bytes > 0 && sample.transfer_seconds > 0.0 && sample.http_code == 200)
                        .collect();
                    if values.is_empty() {
                        println!("  {:>2} МБ: нет корректных результатов", size / 1_000_000);
                        continue;
                    }
                    let mut rates: Vec<f64> = values.iter()
                        .map(|sample| sample.bytes as f64 * 8.0 / sample.transfer_seconds / 1_000_000.0)
                        .collect();
                    rates.sort_by(f64::total_cmp);
                    let median_speed = median(&rates);
                    all_rates.push((size, median_speed));
                    let individual = values.iter().map(|sample| {
                        let speed = sample.bytes as f64 * 8.0 / sample.transfer_seconds / 1_000_000.0;
                        format!("{speed:.2}{}", if sample.timed_out { "*" } else { "" })
                    }).collect::<Vec<_>>().join(" / ");
                    println!(
                        "  {:>2} МБ: {} Мбит/с; медиана {:.2} Мбит/с ({:.2} МиБ/с)",
                        size / 1_000_000,
                        individual,
                        median_speed,
                        median_speed / 8.388608
                    );
                }
                if let Some((_, sustained_speed)) = all_rates.iter().find(|(size, _)| *size == 50_000_000) {
                    rankings.push((tag.clone(), *sustained_speed));
                }
            }
            Err(error) => println!("  Ошибка замера: {error}"),
        }
    }

    rankings.sort_by(|a, b| b.1.total_cmp(&a.1));
    println!();
    println!("Рейтинг по медиане загрузки 50 МБ (устойчивая скорость):");
    if rankings.is_empty() {
        println!("  Не удалось получить корректные результаты.");
    } else {
        for (rank, (tag, speed)) in rankings.iter().enumerate() {
            println!("  {}. {} — {:.2} Мбит/с", rank + 1, tag, speed);
        }
    }
    Ok(())
}

#[derive(Debug)]
struct Sample {
    size: u64,
    bytes: u64,
    transfer_seconds: f64,
    http_code: u16,
    timed_out: bool,
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

fn measure_one(documents: &[(String, Value)], tag: &str) -> AppResult<Vec<Sample>> {
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
        let samples = if ready {
            download_series(port)?
        } else {
            return Err("HTTP probe inbound не запустился за 15 секунд".into());
        };

        let _ = child.kill();
        let _ = child.wait();
        samples
    })();

    if let Err(error) = fs::remove_dir_all(&directory) {
        eprintln!("Не удалось удалить временный каталог {}: {error}", directory.display());
    }
    let _ = fs::remove_file(&log_path);
    result
}

fn add_probe_config(documents: &mut Vec<(String, Value)>, tag: &str, port: u16) -> AppResult<()> {
    let routing_index = documents.iter().position(|(_, doc)| doc.get("routing").is_some())
        .ok_or("В подготовленной конфигурации не найден объект routing.")?;
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

fn download_series(port: u16) -> AppResult<Vec<Sample>> {
    let proxy = format!("http://127.0.0.1:{port}");
    let mut command = Command::new(CURL_BIN);
    command.args(["--silent", "--show-error", "--proxy", &proxy, "--max-time", &MAX_TIME.to_string()]);
    // The warm-up and all measured requests run in one curl process. curl keeps
    // its connection cache, allowing subsequent requests to reuse TCP/TLS.
    command.args(["--output", "/dev/null", "--write-out", "WARMUP\n"]);
    command.arg(format!("{TEST_HOST}{WARMUP_BYTES}"));

    let mut expected = Vec::with_capacity(TEST_SIZES.len() * REPEATS);
    for _ in 0..REPEATS {
        for size in TEST_SIZES {
            command.args([
                "--output", "/dev/null",
                "--write-out", "%{size_download}\t%{time_starttransfer}\t%{time_total}\t%{http_code}\t%{exitcode}\n",
            ]);
            command.arg(format!("{TEST_HOST}{size}"));
            expected.push(size);
        }
    }

    let output = command.stdout(Stdio::piped()).stderr(Stdio::null()).output()
        .map_err(|e| format!("Не удалось запустить curl: {e}"))?;
    let text = String::from_utf8_lossy(&output.stdout);
    let mut lines = text.lines();
    let _warmup = lines.next();
    let mut samples = Vec::with_capacity(expected.len());
    for size in expected {
        let line = lines.next().unwrap_or("");
        let mut fields = line.split('\t');
        let bytes = fields.next().unwrap_or("0").parse::<u64>().unwrap_or(0);
        let first_byte_seconds = fields.next().unwrap_or("0").parse::<f64>().unwrap_or(0.0);
        let total_seconds = fields.next().unwrap_or("0").parse::<f64>().unwrap_or(0.0);
        let http_code = fields.next().unwrap_or("000").parse::<u16>().unwrap_or(0);
        let curl_exit = fields.next().unwrap_or("0").parse::<i32>().unwrap_or(-1);
        let transfer_seconds = (total_seconds - first_byte_seconds).max(0.0);
        let timed_out = curl_exit == 28 && bytes > 0 && http_code == 200;
        samples.push(Sample {
            size, bytes, transfer_seconds, http_code, timed_out,
        });
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
