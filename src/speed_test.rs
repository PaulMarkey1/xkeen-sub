use serde_json::{json, Value};
use std::error::Error;
use std::fs;
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

type AppResult<T> = Result<T, Box<dyn Error>>;

const CONFIG_SOURCE: &str = "/opt/etc/xray/configs";
const ASSET_DIR: &str = "/opt/etc/xray/dat";
const XRAY_DEFAULT: &str = "/opt/sbin/xray";
const CURL_BIN: &str = "/opt/bin/curl";
const TEST_URL: &str = "https://speed.cloudflare.com/__down?bytes=10000000";
const MAX_TIME: u64 = 8;

/// Measures real download throughput through each final subscription outbound.
/// Each candidate gets an isolated helper Xray and an HTTP-proxy probe inbound;
/// a temporary first routing rule pins only probe traffic to that outbound.
/// The live Xray, config files and routing API are never modified.
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
    println!("Замер фактической скорости всех подключений.");
    println!("Размер загрузки: 10 МБ; тайм-аут одного подключения: {MAX_TIME} с.");
    println!("Каждое подключение проверяется отдельно, последовательно.");

    let mut results = Vec::with_capacity(candidates.len());
    for (index, tag) in candidates.iter().enumerate() {
        print!("[{}/{}] {}: ", index + 1, candidates.len(), tag);
        match measure_one(documents, tag) {
            Ok(sample) if sample.http_code == 200 && sample.bytes > 0 && sample.seconds > 0.0 => {
                let mbps = sample.bytes as f64 * 8.0 / sample.seconds / 1_000_000.0;
                let mib_s = sample.bytes as f64 / sample.seconds / 1_048_576.0;
                if sample.timed_out {
                    println!("{:.2} Мбит/с ({:.2} МиБ/с, частичная загрузка за тайм-аут)", mbps, mib_s);
                } else {
                    println!("{:.2} Мбит/с ({:.2} МиБ/с)", mbps, mib_s);
                }
                results.push((tag.clone(), mbps));
            }
            Ok(sample) => println!("замер не удался (HTTP {}, получено {} байт)", sample.http_code, sample.bytes),
            Err(error) => println!("ошибка: {error}"),
        }
    }

    results.sort_by(|a, b| b.1.total_cmp(&a.1));
    println!();
    println!("Рейтинг по измеренной скорости:");
    if results.is_empty() {
        println!("  Не удалось получить ни одного корректного результата.");
    } else {
        for (rank, (tag, mbps)) in results.iter().enumerate() {
            println!("  {}. {} — {:.2} Мбит/с", rank + 1, tag, mbps);
        }
        println!();
        println!("Самое быстрое подключение: {} — {:.2} Мбит/с.", results[0].0, results[0].1);
    }
    Ok(())
}

#[derive(Debug)]
struct Sample {
    bytes: u64,
    seconds: f64,
    http_code: u16,
    timed_out: bool,
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
                    if !matches!(protocol, "freedom" | "blackhole" | "dns" | "loopback")
                        && tag != "api"
                    {
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

fn measure_one(documents: &[(String, Value)], tag: &str) -> AppResult<Sample> {
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let directory = PathBuf::from("/opt/tmp").join(format!("xkeen-sub-speed-{}-{nonce}", std::process::id()));
    fs::create_dir_all("/opt/tmp")?;
    fs::create_dir(&directory)?;
    let log_path = PathBuf::from("/tmp").join(format!("xkeen-sub-speed-{}-{nonce}.log", std::process::id()));

    let result = (|| -> AppResult<Sample> {
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
        let sample = if ready {
            download_through_probe(port)?
        } else {
            Err("HTTP probe inbound не запустился за 15 секунд".into())
        };

        let _ = child.kill();
        let _ = child.wait();
        sample
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

fn download_through_probe(port: u16) -> AppResult<Sample> {
    let output = Command::new(CURL_BIN)
        .args([
            "--silent", "--show-error",
            "--proxy", &format!("http://127.0.0.1:{port}"),
            "--output", "/dev/null",
            "--max-time", &MAX_TIME.to_string(),
            "--write-out", "%{size_download}\n%{time_total}\n%{http_code}\n",
            TEST_URL
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .map_err(|e| format!("Не удалось запустить curl: {e}"))?;
    let text = String::from_utf8_lossy(&output.stdout);
    let mut lines = text.lines();
    let bytes = lines.next().unwrap_or("0").parse::<u64>().unwrap_or(0);
    let seconds = lines.next().unwrap_or("0").parse::<f64>().unwrap_or(0.0);
    let http_code = lines.next().unwrap_or("000").parse::<u16>().unwrap_or(0);
    Ok(Sample {
        bytes,
        seconds,
        http_code,
        timed_out: !output.status.success() && bytes > 0 && http_code == 200,
    })
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
