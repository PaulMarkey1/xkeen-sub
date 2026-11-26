use serde_json::Value;
use std::collections::BTreeMap;
use std::error::Error;
use std::fs;
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::speed_test::{add_probe_configs, eligible_tags, finish_child, reserve_port, safe_config_name, wait_for_ports};
use crate::terminal::{title, warning};
use crate::update_config::{load_config_documents, OUTBOUND_FILE};
use crate::update_lock::FileLock;

type AppResult<T> = Result<T, Box<dyn Error>>;
const ASSET_DIR: &str = "/opt/etc/xray/dat";
const XRAY_DEFAULT: &str = "/opt/sbin/xray";
const CURL_BIN: &str = "/opt/bin/curl";
const TEST_HOST: &str = "https://speed.cloudflare.com/__down?bytes=";
const LOCK: &str = "/opt/var/run/xkeen-sub-update.lock";
const LOG_DIR: &str = "/opt/tmp";

#[derive(Clone, Copy)]
enum Variant { Original, Coach }

#[derive(Clone, Debug)]
struct Sample {
    tag: String,
    stage: &'static str,
    status: String,
    bytes: u64,
    ttfb: f64,
    elapsed: f64,
    mbps: Option<f64>,
    retry_after: Option<Duration>,
}

struct GateState { tokens: f64, updated: Instant, blocked_until: Instant }
struct HostGate { state: Mutex<GateState>, capacity: f64, refill_per_second: f64 }

impl HostGate {
    fn new() -> Self {
        Self { state: Mutex::new(GateState { tokens: 6.0, updated: Instant::now(), blocked_until: Instant::now() }), capacity: 6.0, refill_per_second: 4.0 }
    }
    fn acquire(&self, deadline: Instant) -> bool {
        loop {
            let wait = {
                let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
                let now = Instant::now();
                if now >= deadline { return false; }
                if now < s.blocked_until {
                    s.blocked_until.duration_since(now)
                } else {
                    let elapsed = now.duration_since(s.updated).as_secs_f64();
                    s.tokens = (s.tokens + elapsed * self.refill_per_second).min(self.capacity);
                    s.updated = now;
                    if s.tokens >= 1.0 { s.tokens -= 1.0; return true; }
                    Duration::from_secs_f64(((1.0 - s.tokens) / self.refill_per_second).max(0.01))
                }
            };
            if Instant::now() + wait >= deadline { return false; }
            thread::sleep(wait.min(Duration::from_millis(250)));
        }
    }
    fn throttle(&self, delay: Duration) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        s.blocked_until = s.blocked_until.max(Instant::now() + delay);
        s.tokens = 0.0;
        s.updated = Instant::now();
    }
}

pub(crate) fn run_original() -> AppResult<()> { run(Variant::Original) }
pub(crate) fn run_coach() -> AppResult<()> { run(Variant::Coach) }

fn run(variant: Variant) -> AppResult<()> {
    let _lock = FileLock::acquire(LOCK)?.ok_or("Обновление уже выполняется. Дождитесь его завершения и повторите тест.")?;
    title(match variant { Variant::Original => "V3 — исходное предложение", Variant::Coach => "V3 — альтернативная схема" });
    warning("Проверяются все доступные подключения из текущего 04_outbounds.json. Рабочие конфигурации и основной Xray не изменяются.");
    let docs = load_config_documents()?;
    let outbound_doc = docs.iter().find(|d| d.file_name == OUTBOUND_FILE).ok_or("Рабочий 04_outbounds.json не найден.")?;
    let outbounds = outbound_doc.value.get("outbounds").and_then(Value::as_array).ok_or("В 04_outbounds.json нет массива outbounds.")?;
    let requested: Vec<String> = outbounds.iter().filter_map(|o| {
        let tag = o.get("tag")?.as_str()?;
        let p = o.get("protocol").and_then(Value::as_str).unwrap_or("");
        if tag != "api" && !matches!(p, "freedom" | "blackhole" | "dns" | "loopback") { Some(tag.to_owned()) } else { None }
    }).collect();
    let candidates = eligible_tags(&docs.iter().map(|d| (d.file_name.clone(), d.value.clone())).collect::<Vec<_>>(), &requested);
    if candidates.is_empty() { println!("Нет доступных подключений для проверки."); return Ok(()); }
    if !Path::new(ASSET_DIR).is_dir() { return Err(format!("Каталог данных Xray не найден: {ASSET_DIR}").into()); }

    let total_started = Instant::now();
    let hard_budget = match variant { Variant::Original => Duration::from_secs(65), Variant::Coach => Duration::from_secs(65) };
    let deadline = Instant::now() + hard_budget;
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    fs::create_dir_all(LOG_DIR)?;
    let dir = PathBuf::from(LOG_DIR).join(format!("xkeen-sub-v3-{}-{nonce}", std::process::id()));
    let log_path = PathBuf::from(LOG_DIR).join(format!("xkeen-sub-v3-{}-{nonce}.csv", std::process::id()));
    fs::create_dir(&dir)?;
    let gate = Arc::new(HostGate::new());
    let mut journal: Vec<Sample> = Vec::new();

    let result = (|| -> AppResult<()> {
        let ports = candidates.iter().map(|_| reserve_port()).collect::<AppResult<Vec<_>>>()?;
        let mut helper_docs: Vec<(String, Value)> = docs.iter().map(|d| (d.file_name.clone(), d.value.clone())).collect();
        add_probe_configs(&mut helper_docs, &candidates, &ports)?;
        for (name, value) in &helper_docs {
            if !safe_config_name(name) { return Err(format!("Недопустимое имя файла: {name}").into()); }
            fs::write(dir.join(name), serde_json::to_vec_pretty(value)?)?;
        }
        let xray = std::env::var_os("XRAY_BIN").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(XRAY_DEFAULT));
        let out = fs::File::create(dir.join("xray.log"))?;
        let err = out.try_clone()?;
        let mut child = Command::new(xray).arg("run").env("XRAY_LOCATION_CONFDIR", &dir).env("XRAY_LOCATION_ASSET", ASSET_DIR)
            .stdout(Stdio::from(out)).stderr(Stdio::from(err)).spawn()?;
        if !wait_for_ports(&ports, &mut child, Duration::from_secs(15)) {
            finish_child(&mut child);
            return Err("Вспомогательный Xray не запустился за 15 секунд.".into());
        }

        let (warmup, stage1, top_stage1, stage2, top_stage2, rounds, stage3, retry_429) = match variant {
            Variant::Original => (Duration::from_millis(500), Duration::from_millis(1500), 6usize, Duration::from_millis(2500), 6usize, 3usize, Duration::from_millis(2500), true),
            Variant::Coach => (Duration::from_millis(500), Duration::from_secs(2), 10usize, Duration::from_millis(2500), 10usize, 3usize, Duration::from_millis(2500), false),
        };
        println!("Подключений: {}. Бюджет: {} с. Тестовый источник: Cloudflare speed.cloudflare.com.", candidates.len(), hard_budget.as_secs());
        println!("Этап 1: все подключения, конкурентность 6, прогрев 0,5 с + замер {} с.", stage1.as_secs_f64());
        println!("Этап 2: топ-{}; прогрев + замер {} с. Этап 3: топ-{}, {} раунда(ов).", top_stage1, stage2.as_secs_f64(), 3, rounds);
        if retry_429 { println!("Режим 429: общий токен-бакет, Retry-After и экспоненциальный повтор в пределах общего дедлайна."); }
        else { println!("Режим 429: без повторов; 429 не считается ошибкой узла и включает защитную паузу для тестового хоста."); }

        // Stage 1: every usable outbound is tested. Six at a time; no candidate is pre-filtered.
        let mut first: Vec<(usize, f64)> = Vec::new();
        for (wave, chunk) in candidates.chunks(6).enumerate() {
            if Instant::now() >= deadline { break; }
            println!("\nЭТАП 1 — волна {} из {}", wave + 1, candidates.chunks(6).len());
            let start = wave * 6;
            let gate_ref = gate.clone();
            let results = thread::scope(|scope| {
                let handles: Vec<_> = chunk.iter().enumerate().map(|(i, tag)| {
                    let g = gate_ref.clone();
                    let p = ports[start + i];
                    let t = tag.clone();
                    scope.spawn(move || measure_with_policy(&t, p, "stage1", warmup, stage1, &g, deadline, retry_429))
                }).collect();
                handles.into_iter().map(|h| h.join().unwrap_or_else(|_| failed_sample("thread_panic"))).collect::<Vec<_>>()
            });
            for (i, sample) in results.into_iter().enumerate() {
                if let Some(v) = sample.mbps { println!("  {} — {:.2} Мбит/с", sample.tag, v); first.push((start + i, v)); }
                else { println!("  {} — {}", sample.tag, sample.status); }
                journal.push(sample);
            }
        }
        first.sort_by(|a,b| b.1.total_cmp(&a.1));
        first.truncate(top_stage1);
        if first.is_empty() { finish_child(&mut child); return Ok(()); }

        // Stage 2: source proposal uses top 6; the alternative deliberately keeps top 10.
        println!("\nЭТАП 2 — уточняющая проверка {} кандидатов.", first.len());
        let mut second: Vec<(usize, f64)> = Vec::new();
        for (idx, _) in &first {
            if Instant::now() >= deadline { break; }
            let s = measure_with_policy(&candidates[*idx], ports[*idx], "stage2", warmup, stage2, &gate, deadline, retry_429);
            if let Some(v) = s.mbps { println!("  {} — {:.2} Мбит/с", s.tag, v); second.push((*idx, v)); }
            else { println!("  {} — {}", s.tag, s.status); }
            journal.push(s);
        }
        second.sort_by(|a,b| b.1.total_cmp(&a.1));
        second.truncate(3);
        if second.is_empty() { finish_child(&mut child); return Ok(()); }

        let finalists: Vec<usize> = second.iter().map(|x| x.0).collect();
        println!("\nФИНАЛИСТЫ: {}", finalists.iter().map(|i| candidates[*i].as_str()).collect::<Vec<_>>().join(", "));
        let mut stage3_values: BTreeMap<String, Vec<f64>> = BTreeMap::new();
        for round in 0..rounds {
            if Instant::now() >= deadline { break; }
            println!("\nЭТАП 3 — раунд {}", round + 1);
            let shift = round % finalists.len();
            for offset in 0..finalists.len() {
                if Instant::now() >= deadline { break; }
                let pos = (shift + offset) % finalists.len();
                let idx = finalists[pos];
                let s = measure_with_policy(&candidates[idx], ports[idx], "stage3", Duration::ZERO, stage3, &gate, deadline, retry_429);
                if let Some(v) = s.mbps {
                    println!("  {} — {:.2} Мбит/с", s.tag, v);
                    stage3_values.entry(s.tag.clone()).or_default().push(v);
                } else { println!("  {} — {}", s.tag, s.status); }
                journal.push(s);
            }
        }

        // A separate 4-second control measurement of the provisional winner.
        let provisional = match variant {
            Variant::Original => stage3_values.iter().max_by(|a,b| median(&a.1).total_cmp(&median(&b.1))).map(|(t,_)| t.clone()),
            Variant::Coach => stage3_values.iter().max_by(|a,b| lower_score(&a.1).total_cmp(&lower_score(&b.1))).map(|(t,_)| t.clone()),
        };
        if let Some(tag) = provisional {
            if let Some(idx) = candidates.iter().position(|x| x == &tag) {
                if Instant::now() < deadline {
                    println!("\nКОНТРОЛЬНЫЙ ЗАМЕР ПОБЕДИТЕЛЯ — {tag}, 4 с.");
                    let s = measure_with_policy(&tag, ports[idx], "control", Duration::ZERO, Duration::from_secs(4), &gate, deadline, retry_429);
                    if let Some(v) = s.mbps { println!("  Контрольная скорость: {:.2} Мбит/с", v); }
                    else { println!("  Контрольный замер: {}", s.status); }
                    journal.push(s);
                }
            }
        }

        println!("\nИТОГОВЫЙ РЕЙТИНГ");
        let mut ranking: Vec<(String, f64, f64, usize, usize)> = stage3_values.iter().filter_map(|(tag, vals)| {
            if variant == Variant::Coach && vals.len() < 3 { return None; }
            let med = median(vals)?;
            let mad = mad(vals, med);
            let node_errors = journal.iter().filter(|s| &s.tag == tag && s.stage == "stage3" && s.status.starts_with("node_")).count();
            let attempts = journal.iter().filter(|s| &s.tag == tag && s.stage == "stage3" && s.status != "server_limit" && s.status != "server_error").count();
            let reliability = if attempts == 0 { 0.0 } else { (vals.len() as f64 / attempts as f64).min(1.0) };
            let score = if variant == Variant::Original { med } else { lower_score(vals) };
            Some((tag.clone(), score, mad, vals.len(), node_errors))
        }).collect();
        ranking.sort_by(|a,b| b.1.total_cmp(&a.1));
        if ranking.is_empty() { println!("Недостаточно данных для рейтинга."); }
        for (n, (tag, score, spread, k, errors)) in ranking.iter().enumerate() {
            println!("  {}. {} — {} {:.2} Мбит/с; MAD {:.2}; успешных {}; ошибок узла {}",
                n+1, tag, if variant == Variant::Original { "медиана" } else { "нижняя оценка" }, score, spread, k, errors);
        }
        println!("Примечание: доля ошибок узла и серверные отказы разделены; 429/5xx не ухудшают рейтинг узла.");
        finish_child(&mut child);
        Ok(())
    })();

    let mut csv = String::from("tag,stage,status,bytes,ttfb_seconds,elapsed_seconds,mbps,retry_after_seconds\n");
    for s in &journal {
        csv.push_str(&format!("{},{},{},{},{:.4},{:.4},{},{}\n",
            csv_field(&s.tag), s.stage, s.status, s.bytes, s.ttfb, s.elapsed,
            s.mbps.map(|v| format!("{v:.4}")).unwrap_or_default(),
            s.retry_after.map(|d| d.as_secs_f64().to_string()).unwrap_or_default()));
    }
    fs::write(&log_path, csv)?;
    if let Err(e) = fs::remove_dir_all(&dir) { eprintln!("Не удалось удалить временный каталог: {e}"); }
    println!("\nЖурнал измерений: {}", log_path.display());
    println!("Общее время: {:.2} с.", total_started.elapsed().as_secs_f64());
    result
}

fn measure_with_policy(tag: &str, port: u16, stage: &'static str, warmup: Duration, sample: Duration, gate: &HostGate, deadline: Instant, retry_429: bool) -> Sample {
    let mut attempt = 0u32;
    loop {
        if !gate.acquire(deadline) { return failed_sample_for(tag, stage, "deadline"); }
        let mut s = measure_once(tag, port, stage, warmup, sample);
        if s.status != "server_limit" { return s; }
        if !retry_429 || attempt >= 1 { gate.throttle(Duration::from_secs(2)); return s; }
        let delay = s.retry_after.unwrap_or_else(|| Duration::from_millis(500u64.saturating_mul(2u64.pow(attempt))));
        gate.throttle(delay);
        if Instant::now() + delay >= deadline { return s; }
        thread::sleep(delay);
        attempt += 1;
    }
}

fn measure_once(tag: &str, port: u16, stage: &'static str, warmup: Duration, sample: Duration) -> Sample {
    let proxy = format!("http://127.0.0.1:{port}");
    let id = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let headers = format!("/tmp/xkeen-sub-v3-{}-{id}.headers", std::process::id());
    let warm_secs = warmup.as_secs_f64().max(0.01);
    let measure_secs = sample.as_secs_f64().max(0.01) + 1.0;
    let warm_timeout = format!("{warm_secs:.3}");
    let measure_timeout = format!("{measure_secs:.3}");
    let warm_write = "W|%{size_download}|%{time_starttransfer}|%{time_total}|%{http_code}|%{exitcode}\\n";
    let measure_write = "M|%{size_download}|%{time_starttransfer}|%{time_total}|%{http_code}|%{exitcode}\\n";
    let output = Command::new(CURL_BIN)
        .args(["--silent","--show-error","--proxy",&proxy,"--max-time",&warm_timeout,"--output","/dev/null","--dump-header",&headers,"--write-out",warm_write])
        .arg(format!("{TEST_HOST}256000"))
        .args(["--next","--silent","--show-error","--proxy",&proxy,"--max-time",&measure_timeout,"--output","/dev/null","--dump-header",&headers,"--write-out",measure_write])
        .arg(format!("{TEST_HOST}1000000000"))
        .stdout(Stdio::piped()).stderr(Stdio::piped()).output();
    let output = match output {
        Ok(v) => v,
        Err(e) => { let _ = fs::remove_file(&headers); return failed_sample_for(tag, stage, &format!("measure_tool:{e}")); }
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let lines: Vec<&str> = stdout.lines().collect();
    let fields = lines.iter().find(|l| l.starts_with("M|")).unwrap_or(&"").split('|').collect::<Vec<_>>();
    let bytes = fields.get(1).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
    let ttfb = fields.get(2).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
    let elapsed = fields.get(3).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
    let code = fields.get(4).and_then(|v| v.parse::<u16>().ok()).unwrap_or(0);
    let exit = fields.get(5).and_then(|v| v.parse::<i32>().ok()).unwrap_or(-1);
    let header_text = fs::read_to_string(&headers).unwrap_or_default();
    let _ = fs::remove_file(&headers);
    let retry_after = parse_retry_after(&header_text);
    let (status, mbps) = if code == 429 {
        ("server_limit".to_owned(), None)
    } else if (500..600).contains(&code) {
        ("server_error".to_owned(), None)
    } else if code != 200 {
        (if code == 0 { format!("node_http_or_transport_{exit}") } else { format!("http_{code}") }, None)
    } else if bytes == 0 || elapsed <= ttfb {
        (if exit == 28 { "node_timeout_no_payload".into() } else { "node_no_payload".into() }, None)
    } else if exit != 0 && exit != 28 {
        (format!("node_curl_{exit}"), None)
    } else {
        // curl's max-time bounds the sample; partial body on exit 28 is a valid timed sample.
        ("ok".to_owned(), Some(bytes as f64 * 8.0 / (elapsed - ttfb) / 1_000_000.0))
    };
    if !stderr.trim().is_empty() && status != "ok" && status != "server_limit" && status != "server_error" {
        return Sample { tag: tag.into(), stage, status: format!("{status}:{}", stderr.trim().replace(',', ";")), bytes, ttfb, elapsed, mbps, retry_after };
    }
    Sample { tag: tag.into(), stage, status, bytes, ttfb, elapsed, mbps, retry_after }
}

fn parse_retry_after(headers: &str) -> Option<Duration> {
    headers.lines().rev().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        if name.eq_ignore_ascii_case("retry-after") {
            value.trim().parse::<u64>().ok().map(Duration::from_secs)
        } else { None }
    })
}
fn failed_sample(tag: &str) -> Sample { failed_sample_for(tag, "stage1", "thread_panic") }
fn failed_sample_for(tag: &str, stage: &'static str, status: &str) -> Sample {
    Sample { tag: tag.into(), stage, status: status.into(), bytes: 0, ttfb: 0.0, elapsed: 0.0, mbps: None, retry_after: None }
}
fn median(v: &[f64]) -> Option<f64> {
    if v.is_empty() { return None; }
    let mut x = v.to_vec(); x.sort_by(f64::total_cmp);
    Some(if x.len()%2 == 1 { x[x.len()/2] } else { (x[x.len()/2-1]+x[x.len()/2])/2.0 })
}
fn mad(v: &[f64], med: f64) -> f64 {
    let mut d: Vec<f64> = v.iter().map(|x| (x-med).abs()).collect(); d.sort_by(f64::total_cmp);
    median(&d).unwrap_or(0.0)
}
fn lower_score(v: &[f64]) -> f64 {
    let Some(m) = median(v) else { return 0.0; };
    let spread = mad(v,m);
    m - 1.5 * spread / (v.len() as f64).sqrt()
}
fn csv_field(s: &str) -> String { format!("\"{}\"", s.replace('"', "\"\"")) }

