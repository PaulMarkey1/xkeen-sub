use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::error::Error;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::net::TcpListener;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

type AppResult<T> = Result<T, Box<dyn Error>>;

const CONFIG_SOURCE: &str = "/opt/etc/xray/configs";
const ASSET_DIR: &str = "/opt/etc/xray/dat";
const XRAY_DEFAULT: &str = "/opt/sbin/xray";
const CURL_BIN: &str = "/opt/bin/curl";
const MAX_WAIT: Duration = Duration::from_secs(300);
const POLL_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Default)]
struct NodeStatus {
    tag: String,
    alive: bool,
    delay_ms: u64,
    last_try: u64,
    error: String,
}

/// Selects up to two fastest successful outbounds, probing in blocks of five.
/// Remaining outbounds are deliberately left for the caller to place in the rest balancer.
pub(crate) fn select_advanced_outbounds(
    documents: &[(String, Value)],
    tags: &[String],
) -> AppResult<Vec<(String, u64)>> {
    let mut successful = BTreeMap::<String, u64>::new();

    for (block_index, group) in tags.chunks(5).enumerate() {
        let start = block_index * 5 + 1;
        let end = start + group.len() - 1;
        println!();
        println!("Проверка блока {start}–{end} из {} подключений:", tags.len());

        let statuses = probe_group(documents, group)?;
        for tag in group {
            let status = statuses.get(tag);
            let description = match status {
                Some(status) if status.alive && status.last_try > 0 => {
                    successful.insert(tag.clone(), status.delay_ms);
                    format!("{} мс", status.delay_ms)
                }
                Some(status) if status.last_try > 0 && !status.error.is_empty() => {
                    format!("нет ответа ({})", status.error)
                }
                Some(status) if status.last_try > 0 => "нет ответа".to_owned(),
                _ => "результат не получен".to_owned(),
            };
            println!("  {tag} — {description}");
        }

        let mut ranked: Vec<(String, u64)> = successful
            .iter()
            .map(|(tag, delay)| (tag.clone(), *delay))
            .collect();
        ranked.sort_by_key(|(_, delay)| *delay);

        if ranked.len() >= 2 {
            println!("Найдены два рабочих подключения. Дальнейшие блоки проверять не требуется.");
            break;
        }
        if block_index + 1 < tags.chunks(5).len() {
            println!("Пока найдено рабочих подключений: {}. Проверяем следующий блок.", ranked.len());
        }
    }

    let mut ranked: Vec<(String, u64)> = successful
        .into_iter()
        .collect();
    ranked.sort_by_key(|(_, delay)| *delay);
    ranked.truncate(2);

    println!();
    match ranked.as_slice() {
        [(tag, delay), (reserve, reserve_delay)] => {
            println!("Основное подключение: {tag} — {delay} мс.");
            println!("Резервное подключение: {reserve} — {reserve_delay} мс.");
        }
        [(tag, delay)] => {
            println!("Найдено одно рабочее подключение: {tag} — {delay} мс. Следующий блоков с результатом нет.");
        }
        [] => println!("Рабочих подключений не найдено."),
        _ => unreachable!("ranked is truncated to at most two entries"),
    }

    Ok(ranked)
}

fn probe_group(documents: &[(String, Value)], tags: &[String]) -> AppResult<BTreeMap<String, NodeStatus>> {
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let directory = PathBuf::from("/opt/tmp").join(format!(
        "xkeen-sub-test-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all("/opt/tmp")?;
    fs::create_dir(&directory)?;
    let log_path = PathBuf::from("/tmp").join(format!(
        "xkeen-sub-test-{}-{nonce}.log",
        std::process::id()
    ));

    let result = (|| -> AppResult<BTreeMap<String, NodeStatus>> {
        let api_port = reserve_api_port()?;
        let mut helper_documents = documents.to_vec();
        configure_helper(&mut helper_documents, tags, api_port)?;

        for (name, value) in &helper_documents {
            if !safe_config_name(name) {
                return Err(format!("Недопустимое имя конфигурационного файла: {name}").into());
            }
            let path = directory.join(name);
            let bytes = serde_json::to_vec_pretty(value)?;
            fs::write(path, bytes)?;
        }

        let xray_bin = std::env::var_os("XRAY_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(XRAY_DEFAULT));
        let log = File::create(&log_path)?;
        let stderr = log.try_clone()?;
        let child = Command::new(&xray_bin)
            .arg("run")
            .env("XRAY_LOCATION_CONFDIR", &directory)
            .env("XRAY_LOCATION_ASSET", ASSET_DIR)
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(stderr))
            .spawn()
            .map_err(|error| format!("Не удалось запустить вспомогательный Xray {}: {error}", xray_bin.display()))?;
        let mut process = HelperProcess { child: Some(child) };

        let started = Instant::now();
        let mut latest = BTreeMap::new();
        let mut last_api_error = String::new();

        loop {
            if let Some(child) = process.child.as_mut() {
                if let Some(status) = child.try_wait()? {
                    return Err(format!(
                        "Вспомогательный Xray завершился до получения результатов ({status}). Лог: {}",
                        log_path.display()
                    ).into());
                }
            }

            match request_status(api_port) {
                Ok(statuses) => {
                    latest = statuses.into_iter().map(|status| (status.tag.clone(), status)).collect();
                    if tags.iter().all(|tag| latest.get(tag).map(|status| status.last_try > 0).unwrap_or(false)) {
                        break;
                    }
                    last_api_error.clear();
                }
                Err(error) => last_api_error = error,
            }

            if started.elapsed() >= MAX_WAIT {
                if !last_api_error.is_empty() && latest.is_empty() {
                    return Err(format!(
                        "API вспомогательного Xray не ответил за {} секунд: {}. Лог: {}",
                        MAX_WAIT.as_secs(), last_api_error, log_path.display()
                    ).into());
                }
                break;
            }
            thread::sleep(POLL_INTERVAL);
        }

        Ok(latest)
    })();

    // The child must be stopped before its temporary config directory is removed.
    let cleanup_error = fs::remove_dir_all(&directory).err();
    if let Some(error) = cleanup_error {
        eprintln!("Не удалось удалить временный каталог {}: {error}", directory.display());
    }
    result
}

fn configure_helper(documents: &mut Vec<(String, Value)>, tags: &[String], api_port: u16) -> AppResult<()> {
    // Older xkeen-sub installations may not have an API section at all.
    // The probe runs from an isolated temporary config directory, so provide
    // a complete API endpoint there without modifying the user's live files.
    if !documents.iter().any(|(_, value)| value.get("api").is_some()) {
        documents.push((
            "xkeen-sub-probe-api.json".to_owned(),
            json!({
                "api": {
                    "tag": "api",
                    "services": ["ObservatoryService"]
                },
                "inbounds": [{
                    "tag": "api",
                    "listen": "127.0.0.1",
                    "port": api_port,
                    "protocol": "dokodemo-door",
                    "settings": {"address": "127.0.0.1"}
                }],
                "outbounds": [{
                    "tag": "api",
                    "protocol": "freedom"
                }],
                "routing": {
                    "rules": [{
                        "type": "field",
                        "inboundTag": ["api"],
                        "outboundTag": "api"
                    }]
                }
            }),
        ));
    }
    let api_index = documents.iter().position(|(_, value)| value.get("api").is_some())
        .ok_or("Не удалось подготовить API для вспомогательного Xray.")?;

    {
        let api = documents[api_index].1.get_mut("api")
            .and_then(Value::as_object_mut)
            .ok_or("Объект api должен быть JSON-объектом.")?;
        let services = api.entry("services").or_insert_with(|| json!([]));
        let services = services.as_array_mut()
            .ok_or("Поле api.services должно быть массивом.")?;
        if !services.iter().any(|value| value.as_str() == Some("ObservatoryService")) {
            services.push(Value::String("ObservatoryService".to_owned()));
        }
    }

    let mut api_inbound_found = false;
    for (_, document) in documents.iter_mut() {
        let Some(inbounds) = document.get_mut("inbounds").and_then(Value::as_array_mut) else {
            continue;
        };
        let mut retained = Vec::new();
        for mut inbound in std::mem::take(inbounds) {
            if inbound.get("tag").and_then(Value::as_str) == Some("api") {
                let object = inbound.as_object_mut().ok_or("API inbound должен быть объектом.")?;
                object.insert("listen".to_owned(), Value::String("127.0.0.1".to_owned()));
                object.insert("port".to_owned(), json!(api_port));
                object.insert("protocol".to_owned(), Value::String("dokodemo-door".to_owned()));
                object.insert("settings".to_owned(), json!({"address":"127.0.0.1"}));
                retained.push(inbound);
                api_inbound_found = true;
            }
        }
        *inbounds = retained;
    }

    if !api_inbound_found {
        let inbounds = documents[api_index].1.as_object_mut()
            .ok_or("Корень API-конфигурации должен быть объектом.")?
            .entry("inbounds").or_insert_with(|| json!([]))
            .as_array_mut().ok_or("Поле inbounds должно быть массивом.")?;
        inbounds.push(json!({
            "tag": "api",
            "listen": "127.0.0.1",
            "port": api_port,
            "protocol": "dokodemo-door",
            "settings": {"address": "127.0.0.1"}
        }));
    }

    let selector = Value::Array(tags.iter().cloned().map(Value::String).collect());
    let mut found_observatory = false;
    for (_, document) in documents.iter_mut() {
        let Some(root) = document.as_object_mut() else { continue };
        for key in ["observatory", "burstObservatory", "burstobservatory"] {
            if let Some(value) = root.get_mut(key) {
                let object = value.as_object_mut()
                    .ok_or_else(|| format!("{key} должен быть JSON-объектом."))?;
                object.insert("subjectSelector".to_owned(), selector.clone());
                object.insert("enableConcurrency".to_owned(), json!(true));
                found_observatory = true;
            }
        }
    }
    if !found_observatory {
        documents.push((
            "observatory.json".to_owned(),
            json!({"observatory": {
                "subjectSelector": selector,
                "probeUrl": "https://www.google.com/generate_204",
                "probeInterval": "60s",
                "enableConcurrency": true
            }}),
        ));
    }
    Ok(())
}

fn reserve_api_port() -> AppResult<u16> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

fn request_status(api_port: u16) -> Result<Vec<NodeStatus>, String> {
    let mut child = Command::new(CURL_BIN)
        .args([
            "--silent", "--show-error", "--http2-prior-knowledge",
            "--max-time", "15",
            "-H", "content-type: application/grpc",
            "-H", "te: trailers",
            "--data-binary", "@-",
            &format!("http://127.0.0.1:{api_port}/xray.core.app.observatory.command.ObservatoryService/GetOutboundStatus"),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("Не удалось запустить curl для запроса Observatory API: {error}"))?;

    child.stdin.take()
        .ok_or("Не удалось открыть stdin curl.")?
        .write_all(&[0, 0, 0, 0, 0])
        .map_err(|error| format!("Не удалось отправить запрос Observatory API: {error}"))?;
    let output = child.wait_with_output()
        .map_err(|error| format!("Не удалось получить ответ Observatory API: {error}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
    }
    decode_grpc_response(&output.stdout)
}

fn decode_grpc_response(bytes: &[u8]) -> Result<Vec<NodeStatus>, String> {
    if bytes.len() < 5 || bytes[0] != 0 {
        return Err("Некорректный gRPC-ответ Observatory.".to_owned());
    }
    let frame_len = u32::from_be_bytes([bytes[1], bytes[2], bytes[3], bytes[4]]) as usize;
    if bytes.len() < 5 + frame_len {
        return Err("Неполный gRPC-ответ Observatory.".to_owned());
    }
    let mut result = Vec::new();
    for (field, wire, payload) in protobuf_fields(&bytes[5..5 + frame_len])? {
        if field != 1 || wire != 2 { continue; }
        for (nested_field, nested_wire, nested_payload) in protobuf_fields(payload)? {
            if nested_field == 1 && nested_wire == 2 {
                result.push(parse_outbound_status(nested_payload)?);
            }
        }
    }
    Ok(result)
}

fn parse_outbound_status(bytes: &[u8]) -> Result<NodeStatus, String> {
    let mut status = NodeStatus::default();
    for (field, wire, payload) in protobuf_fields(bytes)? {
        match (field, wire) {
            (1, 0) => status.alive = decode_varint_payload(payload) != 0,
            (2, 0) => status.delay_ms = decode_varint_payload(payload),
            (3, 2) => status.error = String::from_utf8_lossy(payload).into_owned(),
            (4, 2) => status.tag = String::from_utf8_lossy(payload).into_owned(),
            (6, 0) => status.last_try = decode_varint_payload(payload),
            _ => {}
        }
    }
    Ok(status)
}

fn protobuf_fields(bytes: &[u8]) -> Result<Vec<(u64, u8, &[u8])>, String> {
    let mut fields = Vec::new();
    let mut offset = 0usize;
    while offset < bytes.len() {
        let key = read_varint(bytes, &mut offset)
            .ok_or("Некорректный ключ protobuf.")?;
        let field = key >> 3;
        let wire = (key & 7) as u8;
        if field == 0 {
            return Err("Некорректный номер поля protobuf.".to_owned());
        }
        match wire {
            0 => {
                let start = offset;
                read_varint(bytes, &mut offset).ok_or("Некорректное varint-поле protobuf.")?;
                fields.push((field, wire, &bytes[start..offset]));
            }
            1 => {
                let end = offset.checked_add(8).filter(|end| *end <= bytes.len())
                    .ok_or("Обрезанное fixed64-поле protobuf.")?;
                fields.push((field, wire, &bytes[offset..end]));
                offset = end;
            }
            2 => {
                let length = read_varint(bytes, &mut offset)
                    .ok_or("Некорректная длина protobuf-поля.")? as usize;
                let end = offset.checked_add(length).filter(|end| *end <= bytes.len())
                    .ok_or("Обрезанное length-delimited поле protobuf.")?;
                fields.push((field, wire, &bytes[offset..end]));
                offset = end;
            }
            5 => {
                let end = offset.checked_add(4).filter(|end| *end <= bytes.len())
                    .ok_or("Обрезанное fixed32-поле protobuf.")?;
                fields.push((field, wire, &bytes[offset..end]));
                offset = end;
            }
            _ => return Err(format!("Неподдерживаемый wire type protobuf: {wire}.")),
        }
    }
    Ok(fields)
}

fn decode_varint_payload(bytes: &[u8]) -> u64 {
    let mut offset = 0usize;
    read_varint(bytes, &mut offset).unwrap_or(0)
}

fn read_varint(bytes: &[u8], offset: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    let mut shift = 0u32;
    while *offset < bytes.len() && shift < 64 {
        let byte = bytes[*offset];
        *offset += 1;
        value |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
        shift += 7;
    }
    None
}

fn safe_config_name(name: &str) -> bool {
    let path = Path::new(name);
    path.components().count() == 1
        && path.file_name().and_then(|value| value.to_str()) == Some(name)
        && matches!(path.extension().and_then(|value| value.to_str()), Some("json" | "jsonc"))
}

struct HelperProcess {
    child: Option<Child>,
}

impl Drop for HelperProcess {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
