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
const STAGE_SIZES: [u64; 3] = [512_000, 1_000_000, 2_000_000];
const STAGE_TIMEOUTS: [u64; 3] = [3, 5, 8];
const FIRST_STAGE_BATCH: usize = 5;
const SECOND_STAGE_TOP: usize = 6;
const FINAL_STAGE_TOP: usize = 3;
const TOTAL_DEADLINE: Duration = Duration::from_secs(60);

/// Selects one outbound using staged parallel throughput tests.
/// One temporary Xray instance serves all candidates; active Xray/configs are untouched.
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
    println!("Быстрый отбор подключений — лимит 60 секунд.");
    println!("Этап 1: {} подключений, 512 КБ, группы до {} (тайм-аут 3 с).", candidates.len(), FIRST_STAGE_BATCH);
    println!("Этап 2: до {} лучших, 1 МБ (тайм-аут 5 с).", SECOND_STAGE_TOP);
    println!("Этап 3: до {} финалистов, 2 МБ (тайм-аут 8 с).", FINAL_STAGE_TOP);

    let started = Instant::now();
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let directory = PathBuf::from("/opt/tmp").join(format!("xkeen-sub-speed-{}-{nonce}", std::process::id()));
    let log_path = PathBuf::from("/tmp").join(format!("xkeen-sub-speed-{}-{nonce}.log", std::process::id()));
    fs::create_dir_all("/opt/tmp")?;
    fs::create_dir(&directory)?;

    let result = (|| -> AppResult<()> {
        let ports = candidates.iter().map(|_| reserve_port()).collect::<AppResult<Vec<_>>>()?;
        let mut helper_documents = documents.to_vec();
        add_probe_configs(&mut helper_documents, &candidates, &ports)?;
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

        if !wait_for_ports(&ports, &mut child, Duration::from_secs(15)) {
            finish_child(&mut child);
            return Err("Вспомогательный Xray не запустился за 15 секунд.".into());
        }

        println!();
        println!("ЭТАП 1/3 — быстрый отсев");
        let mut current: Vec<(usize, f64)> = Vec::new();
        for (batch_index, batch) in candidates.chunks(FIRST_STAGE_BATCH).enumerate() {
            let remaining = remaining_seconds(started);
            if remaining == 0 { break; }
            let offset = batch_index * FIRST_STAGE_BATCH;
            let results = run_parallel_batch(
                batch,
                &ports[offset..offset + batch.len()],
                STAGE_SIZES[0],
                STAGE_TIMEOUTS[0].min(remaining),
            );
            for (local_index, speed) in results.into_iter().enumerate() {
                let index = offset + local_index;
                if let Some(speed) = speed {
                    println!("  {} — {:.2} Мбит/с", candidates[index], speed);
                    current.push((index, speed));
                } else {
                    println!("  {} — нет результата", candidates[index]);
                }
            }
        }
        current.sort_by(|a, b| b.1.total_cmp(&a.1));
        current.truncate(SECOND_STAGE_TOP);
        if current.is_empty() {
            println!("На первом этапе не получено успешных результатов.");
            finish_child(&mut child);
            return Ok(());
        }

        println!();
        println!("ЭТАП 2/3 — уточнение ({} подключений)", current.len());
        let indices: Vec<usize> = current.iter().map(|v| v.0).collect();
        let tags: Vec<String> = indices.iter().map(|&i| candidates[i].clone()).collect();
        let stage_ports: Vec<u16> = indices.iter().map(|&i| ports[i]).collect();
        let remaining = remaining_seconds(started);
        let results = if remaining > 0 {
            run_parallel_batch(&tags, &stage_ports, STAGE_SIZES[1], STAGE_TIMEOUTS[1].min(remaining))
        } else { vec![None; tags.len()] };
        current = indices.into_iter().zip(results)
            .filter_map(|(i, speed)| speed.map(|s| (i, s))).collect();
        current.sort_by(|a, b| b.1.total_cmp(&a.1));
        current.truncate(FINAL_STAGE_TOP);
        if current.is_empty() {
            println!("На втором этапе не осталось успешных результатов.");
            finish_child(&mut child);
            return Ok(());
        }

        println!();
        println!("ЭТАП 3/3 — финальный выбор ({} подключения)", current.len());
        let indices: Vec<usize> = current.iter().map(|v| v.0).collect();
        let tags: Vec<String> = indices.iter().map(|&i| candidates[i].clone()).collect();
        let stage_ports: Vec<u16> = indices.iter().map(|&i| ports[i]).collect();
        let remaining = remaining_seconds(started);
        let results = if remaining > 0 {
            run_parallel_batch(&tags, &stage_ports, STAGE_SIZES[2], STAGE_TIMEOUTS[2].min(remaining))
        } else { vec![None; tags.len()] };
        let mut ranking: Vec<(usize, f64)> = indices.into_iter().zip(results)
            .filter_map(|(i, speed)| speed.map(|s| (i, s))).collect();
        ranking.sort_by(|a, b| b.1.total_cmp(&a.1));

        println!();
        println!("ИТОГОВЫЙ РЕЙТИНГ — финальная скорость (2 МБ):");
        for (rank, (index, speed)) in ranking.iter().enumerate() {
            println!("  {}. {} — {:.2} Мбит/с", rank + 1, candidates[*index], speed);
        }
        if let Some((index, speed)) = ranking.first() {
            println!();
            println!("Победитель: {} — {:.2} Мбит/с", candidates[*index], speed);
        } else {
            println!("Финальный этап не дал успешных результатов.");
        }
        finish_child(&mut child);
        Ok(())
    })();

    if let Err(error) = fs::remove_dir_all(&directory) {
        eprintln!("Не удалось удалить временный каталог {}: {error}", directory.display());
    }
    let _ = fs::remove_file(&log_path);
    println!();
    println!("Общее время проверки: {:.2} с.", started.elapsed().as_secs_f64());
    result
}

fn remaining_seconds(started: Instant) -> u64 {
    TOTAL_DEADLINE.saturating_sub(started.elapsed()).as_secs()
}

fn finish_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn run_parallel_batch(tags: &[String], ports: &[u16], size: u64, timeout_seconds: u64) -> Vec<Option<f64>> {
    thread::scope(|scope| {
        let handles: Vec<_> = tags.iter().zip(ports.iter()).map(|(tag, port)| {
            scope.spawn(move || measure_candidate(tag, *port, size, timeout_seconds))
        }).collect();
        handles.into_iter().map(|handle| handle.join().ok().flatten()).collect()
    })
}

fn measure_candidate(_tag: &str, port: u16, size: u64, timeout_seconds: u64) -> Option<f64> {
    let proxy = format!("http://127.0.0.1:{port}");
    let timeout = timeout_seconds.to_string();
    let output = Command::new(CURL_BIN)
        .args([
            "--silent", "--show-error", "--proxy", &proxy, "--max-time", &timeout,
            "--output", "/dev/null", "--write-out",
            "%{size_download}\\t%{time_starttransfer}\\t%{time_total}\\t%{http_code}\\t%{exitcode}",
        ])
        .arg(format!("{TEST_HOST}{size}"))
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output().ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let fields: Vec<&str> = stdout.split('\t').map(str::trim).collect();
    if fields.len() != 5 { return None; }
    let bytes = fields[0].parse::<u64>().ok()?;
    let first_byte = fields[1].parse::<f64>().ok()?;
    let total = fields[2].parse::<f64>().ok()?;
    let http_code = fields[3].parse::<u16>().ok()?;
    let curl_exit = fields[4].parse::<i32>().ok()?;
    let transfer = total - first_byte;
    if bytes != size || http_code != 200 || curl_exit != 0 || transfer <= 0.0 { return None; }
    Some(bytes as f64 * 8.0 / transfer / 1_000_000.0)
}

fn eligible_tags(documents: &[(String, Value)], requested: &[String]) -> Vec<String> {
    let mut result = Vec::new();
    for tag in requested {
        let mut eligible = false;
        for (_, document) in documents {
            if let Some(outbounds) = document.get("outbounds").and_then(Value::as_array) {
                for outbound in outbounds {
                    if outbound.get("tag").and_then(Value::as_str) != Some(tag.as_str()) { continue; }
                    let protocol = outbound.get("protocol").and_then(Value::as_str).unwrap_or("");
                    if !matches!(protocol, "freedom" | "blackhole" | "dns" | "loopback") && tag != "api" {
                        eligible = true;
                    }
                }
            }
        }
        if eligible && !result.contains(tag) { result.push(tag.clone()); }
    }
    result
}

fn add_probe_configs(documents: &mut Vec<(String, Value)>, tags: &[String], ports: &[u16]) -> AppResult<()> {
    let routing_index = documents.iter().position(|(_, doc)| doc.get("routing").is_some())
        .ok_or("В рабочей конфигурации не найден объект routing.")?;
    let routing = documents[routing_index].1.get_mut("routing")
        .and_then(Value::as_object_mut).ok_or("Объект routing должен быть JSON-объектом.")?;
    let rules = routing.entry("rules").or_insert_with(|| json!([]))
        .as_array_mut().ok_or("routing.rules должен быть массивом.")?;
    let mut probe_rules = Vec::with_capacity(tags.len());
    for (index, tag) in tags.iter().enumerate() {
        probe_rules.push(json!({
            "type": "field",
            "inboundTag": [format!("xkeen-sub-speed-probe-{index}")],
            "outboundTag": tag
        }));
    }
    for rule in probe_rules.into_iter().rev() { rules.insert(0, rule); }

    let inbound_index = documents.iter().position(|(_, doc)| doc.get("inbounds").is_some())
        .unwrap_or(routing_index);
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
    Ok(())
}

fn reserve_port() -> AppResult<u16> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

fn wait_for_ports(ports: &[u16], child: &mut Child, timeout: Duration) -> bool {
    let started = Instant::now();
    while started.elapsed() < timeout {
        if child.try_wait().ok().flatten().is_some() { return false; }
        if ports.iter().all(|port| TcpStream::connect(("127.0.0.1", *port)).is_ok()) { return true; }
        thread::sleep(Duration::from_millis(100));
    }
    false
}

fn safe_config_name(name: &str) -> bool {
    !name.is_empty() && !name.contains('/') && !name.contains('\\') && name != "." && name != ".."
}
