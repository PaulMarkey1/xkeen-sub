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
const STAGE_SIZES: [u64; 3] = [512_000, 5_000_000, 10_000_000];
const STAGE_TIMEOUTS: [u64; 3] = [3, 7, 15];
const FIRST_STAGE_BATCH: usize = 5;

/// Selects one outbound using staged parallel throughput tests.
/// One temporary Xray instance serves all candidates; active Xray/configs are untouched.
pub(crate) fn rank_outbounds_speed(
    documents: &[(String, Value)],
    tags: &[String],
) -> AppResult<Vec<(String, f64, usize)>> {
    let candidates = eligible_tags(documents, tags);
    if candidates.is_empty() {
        println!("В рабочем 04_outbounds.json нет подключений для замера.");
        return Ok(Vec::new());
    }
    if !Path::new(ASSET_DIR).is_dir() {
        return Err(format!("Каталог данных Xray не найден: {ASSET_DIR}").into());
    }

    println!();
    println!("Тестирование подключений без общего ограничения времени.");
    println!("Этап 1: все {} подключений, 3 прогона, группы по {}, 512 КБ, тайм-аут 3 с.", candidates.len(), FIRST_STAGE_BATCH);
    println!("Этап 2: до 10 подключений, 3 прогона, группы по 5, 5 МБ, тайм-аут 7 с.");
    println!("Этап 3: до 3 финалистов, 3 одновременных прогона по 10 МБ, тайм-аут 15 с.");

    let total_started = Instant::now();
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let directory = PathBuf::from("/opt/tmp").join(format!("xkeen-sub-speed-{}-{nonce}", std::process::id()));
    let log_path = PathBuf::from("/tmp").join(format!("xkeen-sub-speed-{}-{nonce}.log", std::process::id()));
    fs::create_dir_all("/opt/tmp")?;
    fs::create_dir(&directory)?;

    let result = (|| -> AppResult<Vec<(String, f64, usize)>> {
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

        // Stage 1: three rounds, each round processed in batches of five.
        println!();
        println!("ЭТАП 1/3 — первичный отбор");
        let stage_started = Instant::now();
        let mut first_measurements: Vec<Vec<f64>> = vec![Vec::new(); candidates.len()];
        for round in 1..=3 {
            println!("  Прогон {round}/3");
            for (batch_index, batch) in candidates.chunks(FIRST_STAGE_BATCH).enumerate() {
                let offset = batch_index * FIRST_STAGE_BATCH;
                let results = run_parallel_batch(
                    batch,
                    &ports[offset..offset + batch.len()],
                    STAGE_SIZES[0],
                    Duration::from_secs(STAGE_TIMEOUTS[0]),
                );
                for (local_index, speed) in results.into_iter().enumerate() {
                    let index = offset + local_index;
                    if let Some(speed) = speed {
                        first_measurements[index].push(speed);
                        println!("    {} — {:.2} Мбит/с", candidates[index], speed);
                    } else {
                        println!("    {} — нет результата", candidates[index]);
                    }
                }
            }
        }
        let mut first_ranking: Vec<(usize, usize, f64)> = first_measurements.iter().enumerate()
            .filter_map(|(index, values)| median(values).map(|value| (index, values.len(), value)))
            .filter(|(_, successes, _)| *successes >= 2)
            .collect();
        first_ranking.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| b.2.total_cmp(&a.2)));
        first_ranking.truncate(10);
        println!("Время этапа 1: {:.2} с.", stage_started.elapsed().as_secs_f64());
        if first_ranking.is_empty() {
            println!("На первом этапе нет подключений с минимум двумя успешными результатами.");
            finish_child(&mut child);
            return Ok(Vec::new());
        }
        println!("В следующий этап проходят {} подключений.", first_ranking.len());

        // Stage 2: three rounds, batches of five, 5 MB per request.
        println!();
        println!("ЭТАП 2/3 — уточнение ({} подключений, 5 МБ)", first_ranking.len());
        let stage_started = Instant::now();
        let stage2_indices: Vec<usize> = first_ranking.iter().map(|v| v.0).collect();
        let stage2_tags: Vec<String> = stage2_indices.iter().map(|&i| candidates[i].clone()).collect();
        let stage2_ports: Vec<u16> = stage2_indices.iter().map(|&i| ports[i]).collect();
        let mut second_measurements: Vec<Vec<f64>> = vec![Vec::new(); stage2_tags.len()];
        for round in 1..=3 {
            println!("  Прогон {round}/3");
            for batch_start in (0..stage2_tags.len()).step_by(FIRST_STAGE_BATCH) {
                let batch_end = (batch_start + FIRST_STAGE_BATCH).min(stage2_tags.len());
                let results = run_parallel_batch(
                    &stage2_tags[batch_start..batch_end],
                    &stage2_ports[batch_start..batch_end],
                    STAGE_SIZES[1],
                    Duration::from_secs(STAGE_TIMEOUTS[1]),
                );
                for (local_index, speed) in results.into_iter().enumerate() {
                    let index = batch_start + local_index;
                    if let Some(speed) = speed {
                        second_measurements[index].push(speed);
                        println!("    {} — {:.2} Мбит/с", stage2_tags[index], speed);
                    } else {
                        println!("    {} — нет результата", stage2_tags[index]);
                    }
                }
            }
        }
        let mut second_ranking: Vec<(usize, f64, usize)> = second_measurements.iter().enumerate()
            .filter_map(|(index, values)| {
                if values.len() < 2 { return None; }
                median(values).map(|value| (stage2_indices[index], value, values.len()))
            }).collect();
        second_ranking.sort_by(|a, b| b.1.total_cmp(&a.1));
        second_ranking.truncate(3);
        println!("Время этапа 2: {:.2} с.", stage_started.elapsed().as_secs_f64());
        if second_ranking.is_empty() {
            println!("На втором этапе нет подключений с минимум двумя успешными результатами.");
            finish_child(&mut child);
            return Ok(Vec::new());
        }

        // Stage 3: all finalists tested concurrently in each of three rounds.
        println!();
        println!("ЭТАП 3/3 — финальный рейтинг ({} подключения, 10 МБ)", second_ranking.len());
        let stage_started = Instant::now();
        let finalists: Vec<usize> = second_ranking.iter().map(|v| v.0).collect();
        let finalist_tags: Vec<String> = finalists.iter().map(|&i| candidates[i].clone()).collect();
        let finalist_ports: Vec<u16> = finalists.iter().map(|&i| ports[i]).collect();
        let mut final_measurements: Vec<Vec<f64>> = vec![Vec::new(); finalists.len()];
        for round in 1..=3 {
            println!("  Одновременный прогон {round}/3");
            let results = run_parallel_batch(
                &finalist_tags,
                &finalist_ports,
                STAGE_SIZES[2],
                Duration::from_secs(STAGE_TIMEOUTS[2]),
            );
            for (index, speed) in results.into_iter().enumerate() {
                if let Some(speed) = speed {
                    final_measurements[index].push(speed);
                    println!("    {} — {:.2} Мбит/с", finalist_tags[index], speed);
                } else {
                    println!("    {} — нет результата", finalist_tags[index]);
                }
            }
        }
        // Keep every finalist in the result. Candidates with at least two
        // successful measurements are ranked by median speed; candidates
        // without enough measurements remain after them in stage-2 order.
        let mut ranking: Vec<(usize, Option<f64>, usize)> = final_measurements.iter().enumerate()
            .map(|(index, values)| (finalists[index], if values.len() >= 2 { median(values) } else { None }, values.len()))
            .collect();
        ranking.sort_by(|a, b| match (a.1, b.1) {
            (Some(a_speed), Some(b_speed)) => b_speed.total_cmp(&a_speed),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        });
        println!("Время этапа 3: {:.2} с.", stage_started.elapsed().as_secs_f64());

        println!();
        println!("ИТОГОВЫЙ РЕЙТИНГ — медиана замеров (10 МБ):");
        for (rank, (index, speed, count)) in ranking.iter().enumerate() {
            match speed {
                Some(value) => println!("  {}. {} — {:.2} Мбит/с ({}/3 замеров)", rank + 1, candidates[*index], value, count),
                None => println!("  {}. {} — недостаточно успешных замеров ({}/3); место сохранено по результатам этапа 2", rank + 1, candidates[*index], count),
            }
        }
        if let Some((index, Some(speed), count)) = ranking.first() {
            println!();
            println!("Победитель: {} — {:.2} Мбит/с (медиана, {count}/3 замеров)", candidates[*index], speed);
        } else {
            println!();
            println!("Ни один финалист не набрал двух успешных замеров на третьем этапе; порядок сохранён по результатам этапа 2.");
        }
        finish_child(&mut child);
        Ok(ranking.iter().map(|(index, speed, count)| (candidates[*index].clone(), speed.unwrap_or(0.0), *count)).collect())
    })();

    if let Err(error) = fs::remove_dir_all(&directory) {
        eprintln!("Не удалось удалить временный каталог {}: {error}", directory.display());
    }
    let _ = fs::remove_file(&log_path);
    println!();
    println!("Общее время проверки: {:.2} с.", total_started.elapsed().as_secs_f64());
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
