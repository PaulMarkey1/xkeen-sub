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
const SECOND_STAGE_TOP: usize = 6;
const FINAL_STAGE_TOP: usize = 3;
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
    println!("ЭТАП 3 — повторные замеры 10 МБ / 10 с для трёх финалистов до превышения 60 секунд.");
    println!("Итоговые места определяются по медиане успешных замеров этапа 3.");

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
            for (local_index, speed) in results.into_iter().enumerate() {
                let index = offset + local_index;
                if let Some(speed) = speed {
                    println!("  {} — {:.2} Мбит/с", candidates[index], speed);
                    first_stage.push((index, speed));
                } else {
                    println!("  {} — нет результата", candidates[index]);
                }
            }
        }
        first_stage.sort_by(|a, b| b.1.total_cmp(&a.1));
        first_stage.truncate(10);
        println!("После этапа 1 отобрано: {} из 10.", first_stage.len());
        if first_stage.is_empty() {
            finish_child(&mut child);
            return Ok(Vec::new());
        }

        let selected_indices: Vec<usize> = first_stage.iter().map(|item| item.0).collect();
        let selected_tags: Vec<String> = selected_indices.iter().map(|&i| candidates[i].clone()).collect();
        let selected_ports: Vec<u16> = selected_indices.iter().map(|&i| ports[i]).collect();

        // Stage 2a: warm-up. These measurements are deliberately not ranked.
        println!();
        println!("ЭТАП 2 — прогрев 2 МБ для {} подключений (тайм-аут 5 с).", selected_tags.len());
        let warmup = run_parallel_batch(
            &selected_tags,
            &selected_ports,
            2_000_000,
            Duration::from_secs(5),
        );
        for (index, result) in warmup.iter().enumerate() {
            println!("  {} — {}", selected_tags[index], if result.is_some() { "прогрев завершён" } else { "прогрев не пройден" });
        }

        // Stage 2b: one 10 MB measurement chooses the three finalists.
        println!();
        println!("ЭТАП 2 — основной замер 10 МБ для {} подключений (тайм-аут 10 с).", selected_tags.len());
        let stage2 = run_parallel_batch(
            &selected_tags,
            &selected_ports,
            10_000_000,
            Duration::from_secs(10),
        );
        let mut finalists: Vec<(usize, f64)> = Vec::new();
        for (index, speed) in stage2.into_iter().enumerate() {
            if let Some(speed) = speed {
                println!("  {} — {:.2} Мбит/с", selected_tags[index], speed);
                finalists.push((index, speed));
            } else {
                println!("  {} — нет результата", selected_tags[index]);
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

        // Stage 3: repeat 10 MB rounds until a complete round ends at or beyond 60 s.
        let mut measurements: std::collections::BTreeMap<String, Vec<f64>> = std::collections::BTreeMap::new();
        let mut round = 1usize;
        loop {
            println!();
            println!("ЭТАП 3 — повторный замер 10 МБ, раунд {round}.");
            let results = run_parallel_batch(
                &finalist_tags,
                &finalist_ports,
                10_000_000,
                Duration::from_secs(10),
            );
            for (index, speed) in results.into_iter().enumerate() {
                match speed {
                    Some(speed) => {
                        println!("  {} — {:.2} Мбит/с", finalist_tags[index], speed);
                        measurements.entry(finalist_tags[index].clone()).or_default().push(speed);
                    }
                    None => println!("  {} — нет результата", finalist_tags[index]),
                }
            }
            let elapsed = started.elapsed();
            println!("Общее время проверки: {:.2} с.", elapsed.as_secs_f64());
            if elapsed >= TOTAL_DEADLINE {
                break;
            }
            round += 1;
        }

        let mut ranking: Vec<(String, f64)> = measurements.iter()
            .filter_map(|(tag, values)| median(values).map(|value| (tag.clone(), value)))
            .collect();
        ranking.sort_by(|a, b| b.1.total_cmp(&a.1));

        println!();
        println!("ИТОГОВЫЙ РЕЙТИНГ — медиана успешных замеров 10 МБ:");
        for (rank, (tag, value)) in ranking.iter().enumerate() {
            println!("  {}. {} — {:.2} Мбит/с ({} замеров)", rank + 1, tag, value, measurements[tag].len());
        }
        finish_child(&mut child);
        Ok(ranking)
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

fn finish_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn run_parallel_batch(tags: &[String], ports: &[u16], size: u64, timeout: Duration) -> Vec<Option<f64>> {
    thread::scope(|scope| {
        let handles: Vec<_> = tags.iter().zip(ports.iter()).map(|(tag, port)| {
            scope.spawn(move || measure_candidate(tag, *port, size, timeout))
        }).collect();
        handles.into_iter().map(|handle| handle.join().ok().flatten()).collect()
    })
}

fn measure_candidate(_tag: &str, port: u16, size: u64, timeout: Duration) -> Option<f64> {
    let proxy = format!("http://127.0.0.1:{port}");
    let timeout = format!("{:.3}", timeout.as_secs_f64());
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
