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
const FIRST_STAGE_BATCH: usize = 5;
const TOTAL_DEADLINE: Duration = Duration::from_secs(60);

/// Runs the staged throughput selection used by Advanced balancing.
/// A temporary Xray instance serves all candidates; active Xray/configs are untouched.
pub(crate) fn test_outbounds_speed(
    documents: &[(String, Value)],
    tags: &[String],
) -> AppResult<Vec<(String, f64)>> {
    let candidates = eligible_tags(documents, tags);
    if candidates.is_empty() {
        println!("В рабочем 04_outbounds.json нет подключений для замера.");
        return Ok(Vec::new());
    }
    if !Path::new(ASSET_DIR).is_dir() {
        return Err(format!("Каталог данных Xray не найден: {ASSET_DIR}").into());
    }

    println!();
    println!("ЭТАП 1 — быстрый отсев: {} подключений, 512 КБ, группы до 5, тайм-аут 3 с.", candidates.len());
    println!("ЭТАП 2 — прогрев 10 лучших: 2 МБ / 5 с; затем один замер 10 МБ / 10 с.");
    println!("ЭТАП 3 — последовательные повторные замеры 10 МБ / 10 с с чередованием порядка; окно 60 секунд.");
    println!("Рейтинг учитывает медианную скорость и долю успешных попыток.");

    let started = Instant::now();
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let directory = PathBuf::from("/opt/tmp").join(format!("xkeen-sub-speed-{}-{nonce}", std::process::id()));
    let log_path = PathBuf::from("/tmp").join(format!("xkeen-sub-speed-{}-{nonce}.log", std::process::id()));
    fs::create_dir_all("/opt/tmp")?;
    fs::create_dir(&directory)?;

    let result = (|| -> AppResult<Vec<(String, f64)>> {
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
            return Err("Вспомогательный Xray не запустился в отведённое время.".into());
        }

        // Stage 1: test every candidate in batches of at most five.
        let mut first_stage: Vec<(usize, f64)> = Vec::new();
        for (batch_index, batch) in candidates.chunks(FIRST_STAGE_BATCH).enumerate() {
            let offset = batch_index * FIRST_STAGE_BATCH;
            println!();
            println!("ЭТАП 1 — группа {} ({} подключений)", batch_index + 1, batch.len());
            let results = run_parallel_batch(
                batch,
                &ports[offset..offset + batch.len()],
                512_000,
                Duration::from_secs(3),
            );
            for (local_index, measurement) in results.into_iter().enumerate() {
                let index = offset + local_index;
                match measurement {
                    Measurement::Success { speed_mbps } => {
                        println!("  {} — {:.2} Мбит/с", candidates[index], speed_mbps);
                        first_stage.push((index, speed_mbps));
                    }
                    Measurement::Failure(reason) => println!("  {} — нет результата: {}", candidates[index], reason),
                }
            }
        }
        first_stage.sort_by(|a, b| b.1.total_cmp(&a.1));
        first_stage.truncate(10);
        println!("После этапа 1 отобрано подключений: {} (максимум 10).", first_stage.len());
        if first_stage.is_empty() {
            finish_child(&mut child);
            return Ok(Vec::new());
        }

        let selected_indices: Vec<usize> = first_stage.iter().map(|item| item.0).collect();
        let selected_tags: Vec<String> = selected_indices.iter().map(|&i| candidates[i].clone()).collect();
        let selected_ports: Vec<u16> = selected_indices.iter().map(|&i| ports[i]).collect();

        // Stage 2: sequential checks avoid candidates competing for the same WAN link.
        println!();
        println!("ЭТАП 2 — последовательная проверка {} подключений: прогрев 2 МБ / 5 с, затем 10 МБ / 10 с.", selected_tags.len());
        let mut finalists: Vec<(usize, f64)> = Vec::new();
        for index in 0..selected_tags.len() {
            match measure_candidate(&selected_tags[index], selected_ports[index], 2_000_000, Duration::from_secs(5)) {
                Measurement::Success { .. } => println!("  {} — прогрев завершён", selected_tags[index]),
                Measurement::Failure(reason) => println!("  {} — прогрев не пройден: {}", selected_tags[index], reason),
            }
            match measure_candidate(&selected_tags[index], selected_ports[index], 10_000_000, Duration::from_secs(10)) {
                Measurement::Success { speed_mbps, .. } => {
                    println!("  {} — {:.2} Мбит/с", selected_tags[index], speed_mbps);
                    finalists.push((index, speed_mbps));
                }
                Measurement::Failure(reason) => println!("  {} — нет результата: {}", selected_tags[index], reason),
            }
        }
        finalists.sort_by(|a, b| b.1.total_cmp(&a.1));
        finalists.truncate(3);
        if finalists.is_empty() {
            println!("Не удалось определить финалистов: нет успешных замеров 10 МБ.");
            finish_child(&mut child);
            return Ok(Vec::new());
        }

        let finalist_tags: Vec<String> = finalists.iter().map(|(i, _)| selected_tags[*i].clone()).collect();
        let finalist_ports: Vec<u16> = finalists.iter().map(|(i, _)| selected_ports[*i]).collect();
        println!();
        println!("ФИНАЛИСТЫ:");
        for (place, tag) in finalist_tags.iter().enumerate() {
            println!("  {}. {}", place + 1, tag);
        }

        // Stage 3: sequential rounds, rotating the first candidate to reduce order bias.
        // The 60-second window starts here, not during helper startup or earlier stages.
        let stage3_started = Instant::now();
        let mut measurements: std::collections::BTreeMap<String, Vec<f64>> = std::collections::BTreeMap::new();
        let mut failures: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
        let mut round = 1usize;
        loop {
            println!();
            println!("ЭТАП 3 — последовательный повторный замер 10 МБ, раунд {round}.");
            let shift = (round - 1) % finalist_tags.len();
            for offset in 0..finalist_tags.len() {
                let index = (shift + offset) % finalist_tags.len();
                let tag = &finalist_tags[index];
                match measure_candidate(tag, finalist_ports[index], 10_000_000, Duration::from_secs(10)) {
                    Measurement::Success { speed_mbps, .. } => {
                        println!("  {} — {:.2} Мбит/с", tag, speed_mbps);
                        measurements.entry(tag.clone()).or_default().push(speed_mbps);
                    }
                    Measurement::Failure(reason) => {
                        println!("  {} — ошибка: {}", tag, reason);
                        *failures.entry(tag.clone()).or_default() += 1;
                    }
                }
            }
            let elapsed = stage3_started.elapsed();
            println!("Время этапа 3: {:.2} с.", elapsed.as_secs_f64());
            if elapsed >= TOTAL_DEADLINE { break; }
            round += 1;
        }

        // Advanced balancing receives the reliability-adjusted score rather than raw throughput.
        let mut ranking: Vec<(String, f64, f64, f64, usize, usize, f64)> = measurements.iter().filter_map(|(tag, values)| {
            let med = median(values)?;
            let min = values.iter().copied().min_by(f64::total_cmp)?;
            let max = values.iter().copied().max_by(f64::total_cmp)?;
            let failed = *failures.get(tag).unwrap_or(&0);
            let attempts = values.len() + failed;
            let reliability = values.len() as f64 / attempts.max(1) as f64;
            Some((tag.clone(), med, min, max, values.len(), attempts, med * reliability))
        }).collect();
        ranking.sort_by(|a, b| b.6.total_cmp(&a.6).then_with(|| b.1.total_cmp(&a.1)));

        println!();
        println!("ИТОГОВЫЙ РЕЙТИНГ — медиана с поправкой на успешность:");
        for (rank, (tag, med, min, max, success, attempts, score)) in ranking.iter().enumerate() {
            println!("  {}. {} — медиана {:.2}, мин. {:.2}, макс. {:.2} Мбит/с; успешно {}/{}; итоговый балл {:.2}",
                rank + 1, tag, med, min, max, success, attempts, score);
        }
        finish_child(&mut child);
        Ok(ranking.into_iter().map(|(tag, _median, _min, _max, _success, _attempts, score)| (tag, score)).collect())
    })();

    if let Err(error) = fs::remove_dir_all(&directory) {
        eprintln!("Не удалось удалить временный каталог {}: {error}", directory.display());
    }
    let _ = fs::remove_file(&log_path);
    println!();
    println!("Общее время проверки: {:.2} с.", started.elapsed().as_secs_f64());
    result
}

fn median(values: &[f64]) -> Option<f64> {
    if values.is_empty() { return None; }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let middle = sorted.len() / 2;
    if sorted.len() % 2 == 1 {
        Some(sorted[middle])
    } else {
        Some((sorted[middle - 1] + sorted[middle]) / 2.0)
    }
}

pub(crate) fn finish_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

#[derive(Debug)]
enum Measurement {
    Success { speed_mbps: f64 },
    Failure(String),
}

fn run_parallel_batch(tags: &[String], ports: &[u16], size: u64, timeout: Duration) -> Vec<Measurement> {
    thread::scope(|scope| {
        let handles: Vec<_> = tags.iter().zip(ports.iter()).map(|(tag, port)| {
            scope.spawn(move || measure_candidate(tag, *port, size, timeout))
        }).collect();
        handles.into_iter().map(|handle| {
            handle.join().unwrap_or_else(|_| Measurement::Failure("поток проверки завершился аварийно".into()))
        }).collect()
    })
}

fn measure_candidate(_tag: &str, port: u16, size: u64, timeout: Duration) -> Measurement {
    let proxy = format!("http://127.0.0.1:{port}");
    let timeout_arg = format!("{:.3}", timeout.as_secs_f64());
    let output = match Command::new(CURL_BIN)
        .args([
            "--silent", "--show-error", "--proxy", &proxy, "--max-time", &timeout_arg,
            "--output", "/dev/null", "--write-out",
            "%{size_download}\\t%{time_starttransfer}\\t%{time_total}\\t%{http_code}\\t%{exitcode}",
        ])
        .arg(format!("{TEST_HOST}{size}"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output() {
            Ok(output) => output,
            Err(error) => return Measurement::Failure(format!("не удалось запустить curl: {error}")),
        };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    let fields: Vec<&str> = stdout.split('\t').map(str::trim).collect();
    if fields.len() != 5 {
        return Measurement::Failure(if stderr.is_empty() { "curl не вернул статистику".into() } else { stderr });
    }
    let bytes = match fields[0].parse::<u64>() { Ok(v) => v, Err(_) => return Measurement::Failure("не удалось разобрать объём загрузки".into()) };
    let first_byte = match fields[1].parse::<f64>() { Ok(v) => v, Err(_) => return Measurement::Failure("не удалось разобрать время первого байта".into()) };
    let total = match fields[2].parse::<f64>() { Ok(v) => v, Err(_) => return Measurement::Failure("не удалось разобрать время загрузки".into()) };
    let http_code = fields[3].parse::<u16>().unwrap_or(0);
    let curl_exit = fields[4].parse::<i32>().unwrap_or(-1);
    if curl_exit != 0 {
        let reason = match curl_exit {
            5 | 6 => "ошибка DNS/прокси",
            7 => "не удалось подключиться к прокси или серверу",
            28 => "превышен тайм-аут",
            35 | 60 => "ошибка TLS/сертификата",
            52 | 56 => "соединение закрыто или потеряно",
            _ => "ошибка curl",
        };
        return Measurement::Failure(format!("{reason} (curl {curl_exit}, получено {bytes}/{size} байт)"));
    }
    if http_code != 200 { return Measurement::Failure(format!("HTTP {http_code}")); }
    if bytes != size { return Measurement::Failure(format!("неполная загрузка: {bytes}/{size} байт")); }
    let transfer = total - first_byte;
    if transfer <= 0.0 { return Measurement::Failure("некорректное время передачи данных".into()); }
    Measurement::Success { speed_mbps: bytes as f64 * 8.0 / transfer / 1_000_000.0 }
}

pub(crate) fn eligible_tags(documents: &[(String, Value)], requested: &[String]) -> Vec<String> {
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

pub(crate) fn add_probe_configs(documents: &mut Vec<(String, Value)>, tags: &[String], ports: &[u16]) -> AppResult<()> {
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

pub(crate) fn reserve_port() -> AppResult<u16> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

pub(crate) fn wait_for_ports(ports: &[u16], child: &mut Child, timeout: Duration) -> bool {
    let started = Instant::now();
    while started.elapsed() < timeout {
        if child.try_wait().ok().flatten().is_some() { return false; }
        if ports.iter().all(|port| TcpStream::connect(("127.0.0.1", *port)).is_ok()) { return true; }
        thread::sleep(Duration::from_millis(100));
    }
    false
}

pub(crate) fn safe_config_name(name: &str) -> bool {
    !name.is_empty() && !name.contains('/') && !name.contains('\\') && name != "." && name != ".."
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
