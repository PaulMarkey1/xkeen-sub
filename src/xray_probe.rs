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

/// Adds a complete API endpoint to a prepared configuration set.
///
/// When `isolated` is true (the temporary probe instance), the API inbound is
/// moved to the ephemeral loopback port and all non-API inbounds are removed
/// so the helper Xray cannot collide with the running proxy. For the actual
/// configuration, existing valid API settings are preserved.
pub(crate) fn ensure_api_configuration(
    documents: &mut Vec<(String, Value)>,
    api_port: u16,
) -> AppResult<()> {
    ensure_api(documents, api_port, false)
}

fn configure_helper(documents: &mut Vec<(String, Value)>, tags: &[String], api_port: u16) -> AppResult<()> {
    ensure_api(documents, api_port, true)?;

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

fn ensure_api(documents: &mut Vec<(String, Value)>, api_port: u16, isolated: bool) -> AppResult<()> {
    // An API key alone is not sufficient: old installations may contain an
    // empty/partial object without a tag, services, inbound, outbound or route.
    let api_index = match documents.iter().position(|(_, value)| value.get("api").is_some()) {
        Some(index) => index,
        None => {
            let mut name = "xkeen-sub-api.json".to_owned();
            let mut suffix = 1usize;
            while documents.iter().any(|(existing, _)| existing == &name) {
                name = format!("xkeen-sub-api-{suffix}.json");
                suffix += 1;
            }
            documents.push((name, json!({"api": {}})));
            documents.len() - 1
        }
    };

    if !documents[api_index].1.get("api").map(Value::is_object).unwrap_or(false) {
        documents[api_index].1.as_object_mut()
            .ok_or("Корень API-конфигурации должен быть JSON-объектом.")?
            .insert("api".to_owned(), json!({}));
    }

    let existing_api_tag = documents[api_index].1["api"].get("tag")
        .and_then(Value::as_str).filter(|tag| !tag.trim().is_empty()).map(str::to_owned);
    let inbound_tags: Vec<String> = documents.iter()
        .filter_map(|(_, doc)| doc.get("inbounds").and_then(Value::as_array))
        .flatten().filter_map(|item| item.get("tag").and_then(Value::as_str))
        .map(str::to_owned).collect();
    let existing_outbound_tags: Vec<String> = documents.iter()
        .filter_map(|(_, doc)| doc.get("outbounds").and_then(Value::as_array))
        .flatten().filter_map(|item| item.get("tag").and_then(Value::as_str))
        .map(str::to_owned).collect();
    let all_tags: Vec<String> = inbound_tags.iter().chain(existing_outbound_tags.iter()).cloned().collect();
    let previous_api_tag = existing_api_tag
        .or_else(|| inbound_tags.iter().find(|tag| tag.as_str() == "api").cloned());
    let preferred_api_tag = previous_api_tag.clone()
        .unwrap_or_else(|| unique_tag("api", &all_tags));
    let api_tag = if documents.iter().any(|(_, doc)| {
        doc.get("outbounds").and_then(Value::as_array).into_iter().flatten().any(|item|
            item.get("tag").and_then(Value::as_str) == Some(preferred_api_tag.as_str())
                && item.get("protocol").and_then(Value::as_str) != Some("freedom"))
    }) {
        unique_tag("xkeen-sub-api", &all_tags)
    } else {
        preferred_api_tag.clone()
    };
    let effective_api_port = if isolated {
        api_port
    } else {
        available_config_port(documents, api_port, &api_tag)
    };
    documents[api_index].1["api"]["tag"] = Value::String(api_tag.clone());

    let services = documents[api_index].1["api"].get_mut("services")
        .and_then(Value::as_array_mut);
    if services.is_none() {
        documents[api_index].1["api"]["services"] = json!([]);
    }
    let services = documents[api_index].1["api"]["services"].as_array_mut()
        .ok_or("Не удалось подготовить api.services.")?;
    for service in ["RoutingService", "ObservatoryService"] {
        if !services.iter().any(|value| value.as_str() == Some(service)) {
            services.push(Value::String(service.to_owned()));
        }
    }

    let mut api_inbound_found = false;
    for (_, document) in documents.iter_mut() {
        let Some(inbounds) = document.get_mut("inbounds").and_then(Value::as_array_mut) else {
            continue;
        };
        let mut retained_api = None;
        for inbound in inbounds.iter() {
            if inbound.get("tag").and_then(Value::as_str) == Some(api_tag.as_str())
                || previous_api_tag.as_deref().map(|tag| inbound.get("tag").and_then(Value::as_str) == Some(tag)).unwrap_or(false) {
                retained_api = Some(inbound.clone());
                break;
            }
        }
        if let Some(mut inbound) = retained_api {
            if !api_inbound_found {
                let object = inbound.as_object_mut().ok_or("API inbound должен быть объектом.")?;
                object.insert("tag".to_owned(), Value::String(api_tag.clone()));
                object.insert("protocol".to_owned(), Value::String("dokodemo-door".to_owned()));
                object.insert("settings".to_owned(), json!({"address":"127.0.0.1"}));
                if isolated {
                    object.insert("listen".to_owned(), Value::String("127.0.0.1".to_owned()));
                    object.insert("port".to_owned(), json!(effective_api_port));
                } else {
                    if object.get("listen").and_then(Value::as_str).is_none() {
                        object.insert("listen".to_owned(), Value::String("127.0.0.1".to_owned()));
                    }
                    if object.get("port").and_then(Value::as_u64).filter(|port| *port > 0 && *port <= u16::MAX as u64).is_none() {
                        object.insert("port".to_owned(), json!(effective_api_port));
                    }
                }
                let mut rebuilt = Vec::with_capacity(inbounds.len());
                for existing in std::mem::take(inbounds) {
                    if existing.get("tag").and_then(Value::as_str) == Some(api_tag.as_str())
                        || previous_api_tag.as_deref().map(|tag| existing.get("tag").and_then(Value::as_str) == Some(tag)).unwrap_or(false) {
                        if !api_inbound_found {
                            rebuilt.push(inbound.clone());
                            api_inbound_found = true;
                        }
                    } else if !isolated {
                        rebuilt.push(existing);
                    }
                }
                *inbounds = rebuilt;
            } else {
                inbounds.retain(|existing| existing.get("tag").and_then(Value::as_str) != Some(api_tag.as_str())
                    && !previous_api_tag.as_deref().map(|tag| existing.get("tag").and_then(Value::as_str) == Some(tag)).unwrap_or(false));
            }
        } else if isolated {
            inbounds.clear();
        }
    }

    if !api_inbound_found {
        let root = documents[api_index].1.as_object_mut()
            .ok_or("Корень API-конфигурации должен быть объектом.")?;
        if !root.get("inbounds").map(Value::is_array).unwrap_or(false) {
            root.insert("inbounds".to_owned(), json!([]));
        }
        let inbounds = root.get_mut("inbounds").and_then(Value::as_array_mut)
            .ok_or("Не удалось подготовить массив inbounds.")?;
        inbounds.push(json!({
            "tag": api_tag,
            "listen": "127.0.0.1",
            "port": effective_api_port,
            "protocol": "dokodemo-door",
            "settings": {"address":"127.0.0.1"}
        }));
    }

    // The API inbound must be routed to a freedom outbound. Do not overwrite
    // unrelated outbounds or routing rules from a user's existing config.
    let api_outbound_tag = api_tag.clone();
    if !documents.iter().any(|(_, doc)| {
        doc.get("outbounds").and_then(Value::as_array).into_iter().flatten().any(|item|
            item.get("tag").and_then(Value::as_str) == Some(api_outbound_tag.as_str())
                && item.get("protocol").and_then(Value::as_str) == Some("freedom"))
    }) {
        let root = documents[api_index].1.as_object_mut()
            .ok_or("Корень API-конфигурации должен быть объектом.")?;
        if !root.get("outbounds").map(Value::is_array).unwrap_or(false) {
            root.insert("outbounds".to_owned(), json!([]));
        }
        root.get_mut("outbounds").and_then(Value::as_array_mut)
            .ok_or("Не удалось подготовить массив outbounds.")?
            .push(json!({"tag":api_outbound_tag,"protocol":"freedom"}));
    }

    let mut route_found = false;
    for (_, document) in documents.iter_mut() {
        if let Some(rules) = document.get_mut("routing").and_then(|routing| routing.get_mut("rules")).and_then(Value::as_array_mut) {
            if rules.iter().any(|rule| {
                rule.get("inboundTag").and_then(Value::as_array).map(|tags|
                    tags.iter().any(|tag| tag.as_str() == Some(api_tag.as_str()))).unwrap_or(false)
                    && rule.get("outboundTag").and_then(Value::as_str) == Some(api_tag.as_str())
            }) {
                route_found = true;
                break;
            }
        }
    }
    if !route_found {
        let root = documents[api_index].1.as_object_mut()
            .ok_or("Корень API-конфигурации должен быть объектом.")?;
        if !root.get("routing").map(Value::is_object).unwrap_or(false) {
            root.insert("routing".to_owned(), json!({}));
        }
        let routing = root.get_mut("routing").and_then(Value::as_object_mut)
            .ok_or("Не удалось подготовить объект routing.")?;
        if !routing.get("rules").map(Value::is_array).unwrap_or(false) {
            routing.insert("rules".to_owned(), json!([]));
        }
        routing.get_mut("rules").and_then(Value::as_array_mut)
            .ok_or("Не удалось подготовить массив routing.rules.")?
            .insert(0, json!({
                "type":"field",
                "inboundTag":[api_tag],
                "outboundTag":api_tag
            }));
    }

    // In the helper process only the API inbound remains; the active config
    // keeps every user's other inbound untouched.
    if isolated {
        for (_, document) in documents.iter_mut() {
            if let Some(inbounds) = document.get_mut("inbounds").and_then(Value::as_array_mut) {
                inbounds.retain(|inbound| inbound.get("tag").and_then(Value::as_str) == Some(api_tag.as_str()));
            }
        }
    }
    Ok(())
}

fn available_config_port(documents: &[(String, Value)], preferred: u16, api_tag: &str) -> u16 {
    let used: Vec<u16> = documents.iter()
        .filter_map(|(_, doc)| doc.get("inbounds").and_then(Value::as_array))
        .flatten()
        .filter(|inbound| inbound.get("tag").and_then(Value::as_str) != Some(api_tag))
        .filter_map(|inbound| inbound.get("port").and_then(Value::as_u64))
        .filter_map(|port| u16::try_from(port).ok())
        .collect();
    if !used.contains(&preferred) {
        return preferred;
    }
    (10085..=20000).find(|port| !used.contains(port)).unwrap_or(preferred)
}

fn unique_tag(base: &str, existing: &[String]) -> String {
    if !existing.iter().any(|tag| tag == base) {
        return base.to_owned();
    }
    let mut suffix = 1usize;
    loop {
        let candidate = format!("{base}-{suffix}");
        if !existing.iter().any(|tag| tag == &candidate) {
            return candidate;
        }
        suffix += 1;
    }
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

#[cfg(test)]
mod api_tests {
    use super::ensure_api;
    use serde_json::{json, Value};

    #[test]
    fn creates_complete_api_for_legacy_configuration_without_api() {
        let mut docs = vec![(
            "04_outbounds.json".to_owned(),
            json!({"outbounds":[{"tag":"node","protocol":"vless"}]}),
        ), (
            "05_routing.json".to_owned(),
            json!({"routing":{"rules":[]}}),
        )];

        ensure_api(&mut docs, 10085, false).unwrap();

        let api_doc = docs.iter().find(|(_, value)| value.get("api").is_some()).unwrap();
        let tag = api_doc.1["api"]["tag"].as_str().unwrap();
        assert_eq!(api_doc.1["api"]["services"], json!(["RoutingService","ObservatoryService"]));
        assert!(docs.iter().any(|(_, value)| value.get("inbounds").and_then(Value::as_array)
            .map(|items| items.iter().any(|item| item["tag"] == tag && item["port"] == 10085))
            .unwrap_or(false)));
        assert!(docs.iter().any(|(_, value)| value.get("outbounds").and_then(Value::as_array)
            .map(|items| items.iter().any(|item| item["tag"] == "xkeen-sub-api-out" && item["protocol"] == "freedom"))
            .unwrap_or(false)));
        assert!(docs.iter().any(|(_, value)| value.get("routing").and_then(|r| r.get("rules")).and_then(Value::as_array)
            .map(|rules| rules.iter().any(|rule| rule["inboundTag"][0] == tag))
            .unwrap_or(false)));
    }

    #[test]
    fn repairs_partial_api_and_preserves_existing_services_and_inbounds() {
        let mut docs = vec![(
            "00_api.json".to_owned(),
            json!({
                "api": {"services":["StatsService"]},
                "inbounds":[
                    {"tag":"api","listen":"127.0.0.1","port":10086,"protocol":"dokodemo-door","settings":{"address":"127.0.0.1"}},
                    {"tag":"socks-in","port":1080,"protocol":"socks"}
                ]
            }),
        ), (
            "04_outbounds.json".to_owned(),
            json!({"outbounds":[{"tag":"api","protocol":"freedom"}]}),
        ), (
            "05_routing.json".to_owned(),
            json!({"routing":{"rules":[]}}),
        )];

        ensure_api(&mut docs, 10085, false).unwrap();

        assert_eq!(docs[0].1["api"]["tag"], "api");
        assert_eq!(docs[0].1["api"]["services"], json!(["StatsService","RoutingService","ObservatoryService"]));
        assert_eq!(docs[0].1["inbounds"].as_array().unwrap().len(), 2);
        assert_eq!(docs[0].1["inbounds"][0]["port"], 10086);
        assert_eq!(docs[0].1["inbounds"][1]["tag"], "socks-in");
    }

    #[test]
    fn helper_api_is_isolated_and_uses_the_ephemeral_port() {
        let mut docs = vec![(
            "00_api.json".to_owned(),
            json!({"api":{"tag":"api","services":["RoutingService"]},
                "inbounds":[{"tag":"api","port":10085},{"tag":"socks-in","port":1080}]}),
        )];
        ensure_api(&mut docs, 45678, true).unwrap();
        let inbounds = docs[0].1["inbounds"].as_array().unwrap();
        assert_eq!(inbounds.len(), 1);
        assert_eq!(inbounds[0]["tag"], "api");
        assert_eq!(inbounds[0]["listen"], "127.0.0.1");
        assert_eq!(inbounds[0]["port"], 45678);
    }
}
