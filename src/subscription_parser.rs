use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};
use base64::Engine;
use serde_json::{json, Map, Value};
use std::error::Error;
use url::Url;

type AppResult<T> = Result<T, Box<dyn Error>>;

#[derive(Debug, Default)]
pub struct ParsedSubscription {
    pub outbounds: Vec<Value>,
    pub unrecognized: usize,
    pub errors: Vec<String>,
}

/// Accepts either an Xray JSON document or a text/Base64 subscription containing
/// share links. The original bytes are kept separately by the caller for baseline.
pub fn parse_subscription(bytes: &[u8]) -> AppResult<ParsedSubscription> {
    if bytes.is_empty() {
        return Err("Подписка вернула пустое содержимое.".into());
    }

    if let Ok(document) = serde_json::from_slice::<Value>(bytes) {
        if let Some(outbounds) = document.get("outbounds").and_then(Value::as_array) {
            let mut parsed = ParsedSubscription::default();
            for outbound in outbounds {
                if outbound.is_object()
                    && outbound.get("tag").and_then(Value::as_str).is_some_and(|s| !s.is_empty())
                    && outbound.get("protocol").and_then(Value::as_str).is_some()
                {
                    parsed.outbounds.push(outbound.clone());
                } else {
                    parsed.unrecognized += 1;
                    parsed.errors.push("JSON Outbound без корректных Tag или protocol".to_owned());
                }
            }
            if parsed.outbounds.is_empty() {
                return Err("В JSON-подписке нет корректных Outbound.".into());
            }
            return Ok(parsed);
        }
    }

    let mut body = String::from_utf8_lossy(bytes).trim().to_owned();
    if !body.contains("://") {
        if let Some(decoded) = decode_base64(&body) {
            let candidate = String::from_utf8_lossy(&decoded).trim().to_owned();
            if candidate.contains("://") {
                body = candidate;
            }
        }
    }

    let mut parsed = ParsedSubscription::default();
    for line in body.lines().map(str::trim).filter(|line| !line.is_empty()) {
        if line.starts_with('#') || line.starts_with("//") {
            continue;
        }
        if !line.contains("://") {
            continue;
        }
        match parse_proxy_url(line) {
            Ok(outbound) => parsed.outbounds.push(outbound),
            Err(error) => {
                parsed.unrecognized += 1;
                parsed.errors.push(error);
            }
        }
    }

    if parsed.outbounds.is_empty() {
        let detail = parsed.errors.first().cloned().unwrap_or_else(|| {
            "Не найдено ни одного поддерживаемого URI подключения.".to_owned()
        });
        return Err(format!("Подписка не содержит распознанных подключений: {detail}").into());
    }

    deduplicate_tags(&mut parsed.outbounds);
    Ok(parsed)
}

fn decode_base64(input: &str) -> Option<Vec<u8>> {
    let compact: String = input.chars().filter(|c| !c.is_whitespace()).collect();
    [ &STANDARD, &STANDARD_NO_PAD, &URL_SAFE, &URL_SAFE_NO_PAD ]
        .iter()
        .find_map(|encoding| encoding.decode(compact.as_bytes()).ok())
}

fn parse_proxy_url(raw: &str) -> Result<Value, String> {
    let scheme = raw.split_once("://").map(|(s, _)| s.to_ascii_lowercase())
        .ok_or_else(|| "Некорректная ссылка подключения".to_owned())?;
    if scheme == "vmess" {
        return parse_vmess(raw);
    }
    if scheme == "ss" {
        return parse_shadowsocks(raw);
    }

    let url = Url::parse(raw).map_err(|e| format!("Ошибка URL: {e}"))?;
    let host = url.host_str().ok_or_else(|| "В ссылке отсутствует адрес сервера".to_owned())?;
    let port = url.port().unwrap_or(443);
    if port == 0 {
        return Err("Некорректный порт сервера".to_owned());
    }
    let query = url.query_pairs().into_owned().collect::<std::collections::HashMap<_, _>>();
    let get = |key: &str| query.get(key).map(String::as_str).unwrap_or("");
    let tag = make_tag(url.fragment().unwrap_or(""), host, port);

    match scheme.as_str() {
        "vless" => {
            let id = url.username();
            if id.is_empty() {
                return Err("VLESS: отсутствует UUID".to_owned());
            }
            let network = default(get("type"), "tcp");
            if !["tcp", "raw", "ws", "grpc", "xhttp"].contains(&network) {
                return Err(format!("VLESS: неподдерживаемый транспорт {network}"));
            }
            let security = default(get("security"), "none");
            if !["none", "tls", "reality"].contains(&security) {
                return Err(format!("VLESS: неподдерживаемая защита {security}"));
            }
            let user = json!({
                "id": id,
                "encryption": default(get("encryption"), "none"),
                "flow": get("flow")
            });
            let mut outbound = json!({
                "tag": tag,
                "protocol": "vless",
                "settings": {"vnext":[{"address":host,"port":port,"users":[user]}]},
                "streamSettings": stream_settings(
                    network, security, host, get("sni"), get("fp"), get("alpn"),
                    get("allowInsecure") == "1" || get("insecure") == "1",
                    get("pbk"), get("sid"), get("spx"), get("host"), get("path"),
                    get("mode"), get("serviceName")
                )
            });
            if get("encryption") == "" {
                outbound["settings"]["vnext"][0]["users"][0]["encryption"] = json!("none");
            }
            Ok(outbound)
        }
        "trojan" => {
            let password = url.password().or_else(|| {
                let user = url.username();
                if user.is_empty() { None } else { Some(user) }
            }).unwrap_or("");
            if password.is_empty() {
                return Err("Trojan: отсутствует пароль".to_owned());
            }
            let network = default(get("type"), "tcp");
            if !["tcp", "raw", "ws", "grpc"].contains(&network) {
                return Err(format!("Trojan: неподдерживаемый транспорт {network}"));
            }
            let server = json!({"address":host,"port":port,"password":password});
            Ok(json!({
                "tag":tag,"protocol":"trojan","settings":{"servers":[server]},
                "streamSettings":stream_settings(
                    network, default(get("security"), "tls"), host, get("sni"), get("fp"),
                    get("alpn"), get("allowInsecure") == "1" || get("insecure") == "1",
                    "", "", "", get("host"), get("path"), get("mode"), get("serviceName")
                )
            }))
        }
        "hysteria2" | "hy2" => {
            let auth = url.username();
            let sni = default(get("sni"), host);
            Ok(json!({
                "tag":tag,
                "protocol":"hysteria",
                "settings":{"version":2,"address":host,"port":port},
                "streamSettings":{
                    "network":"hysteria","security":"tls",
                    "tlsSettings":{"serverName":sni,"allowInsecure":get("insecure") == "1"},
                    "hysteriaSettings":{"version":2,"auth":auth,"keepAlivePeriod":5}
                }
            }))
        }
        other => Err(format!("Неподдерживаемый протокол: {other}")),
    }
}

fn parse_vmess(raw: &str) -> Result<Value, String> {
    let payload = raw.strip_prefix("vmess://").unwrap_or(raw).split('#').next().unwrap_or("").trim();
    let decoded = decode_base64(payload).ok_or_else(|| "VMess: не удалось декодировать Base64".to_owned())?;
    let data: Value = serde_json::from_slice(&decoded)
        .map_err(|e| format!("VMess: некорректный JSON: {e}"))?;
    let host = string_field(&data, "add");
    let id = string_field(&data, "id");
    if host.is_empty() || id.is_empty() {
        return Err("VMess: отсутствует сервер или UUID".to_owned());
    }
    let port = number_field(&data, "port").unwrap_or(443);
    let network = default(&string_field(&data, "net"), "tcp");
    if !["tcp", "raw", "ws", "grpc"].contains(&network) {
        return Err(format!("VMess: неподдерживаемый транспорт {network}"));
    }
    let tls = string_field(&data, "tls") == "tls";
    let security = if tls { "tls" } else { "none" };
    let path = string_field(&data, "path");
    let service = if network == "grpc" { path.as_str() } else { "" };
    let host_header = string_field(&data, "host");
    let tag = make_tag(
        raw.split_once('#').map(|(_, f)| f).unwrap_or(""),
        &host, port
    );
    let user = json!({
        "id":id,
        "alterId":number_field(&data,"aid").unwrap_or(0),
        "security":default(&string_field(&data,"scy"),"auto")
    });
    Ok(json!({
        "tag":tag,
        "protocol":"vmess",
        "settings":{"vnext":[{"address":host,"port":port,"users":[user]}]},
        "streamSettings":stream_settings(
            network,security,&host,&string_field(&data,"sni"),&string_field(&data,"fp"),
            &string_field(&data,"alpn"),false,"","","",&host_header,&path,"",service
        )
    }))
}

fn parse_shadowsocks(raw: &str) -> Result<Value, String> {
    let rest = raw.strip_prefix("ss://").ok_or_else(|| "Некорректная SS-ссылка".to_owned())?;
    let (without_fragment, fragment) = rest.split_once('#').unwrap_or((rest, ""));
    let (credentials, server) = if let Some((left, right)) = without_fragment.rsplit_once('@') {
        (left, right)
    } else {
        let decoded = decode_base64(without_fragment)
            .ok_or_else(|| "Shadowsocks: не удалось декодировать credentials".to_owned())?;
        let decoded = String::from_utf8(decoded)
            .map_err(|_| "Shadowsocks: credentials не являются UTF-8".to_owned())?;
        let (credentials, server) = decoded.rsplit_once('@')
            .ok_or_else(|| "Shadowsocks: неверный формат ссылки".to_owned())?;
        return build_ss(credentials, server, fragment);
    };
    let decoded_credentials = decode_base64(credentials).unwrap_or_else(|| credentials.as_bytes().to_vec());
    let credentials = String::from_utf8(decoded_credentials)
        .map_err(|_| "Shadowsocks: credentials не являются UTF-8".to_owned())?;
    build_ss(&credentials, server, fragment)
}

fn build_ss(credentials: &str, server: &str, fragment: &str) -> Result<Value, String> {
    let (method, password) = credentials.split_once(':')
        .ok_or_else(|| "Shadowsocks: ожидается method:password".to_owned())?;
    let server_url = Url::parse(&format!("ss://{server}"))
        .map_err(|e| format!("Shadowsocks: ошибка адреса: {e}"))?;
    let host = server_url.host_str().ok_or_else(|| "Shadowsocks: отсутствует сервер".to_owned())?;
    let port = server_url.port().unwrap_or(443);
    Ok(json!({
        "tag":make_tag(fragment,host,port),
        "protocol":"shadowsocks",
        "settings":{"servers":[{"address":host,"port":port,"method":method,"password":password}]}
    }))
}

fn stream_settings(
    network: &str, security: &str, address: &str, sni: &str, fp: &str, alpn: &str,
    insecure: bool, public_key: &str, short_id: &str, spider_x: &str,
    host: &str, path: &str, mode: &str, service_name: &str
) -> Value {
    let mut result = Map::new();
    result.insert("network".to_owned(), json!(network));
    result.insert("security".to_owned(), json!(security));
    if security == "tls" {
        let mut tls = json!({
            "serverName":default(sni,address),
            "allowInsecure":insecure,
            "fingerprint":default(fp,"chrome")
        });
        let protocols: Vec<&str> = alpn.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
        if !protocols.is_empty() { tls["alpn"] = json!(protocols); }
        result.insert("tlsSettings".to_owned(), tls);
    } else if security == "reality" {
        let mut reality = json!({
            "serverName":sni,
            "fingerprint":default(fp,"chrome"),
            "publicKey":public_key
        });
        if !short_id.is_empty() { reality["shortId"] = json!(short_id); }
        if !spider_x.is_empty() { reality["spiderX"] = json!(spider_x); }
        result.insert("realitySettings".to_owned(), reality);
    }
    match network {
        "ws" => {
            let mut settings = json!({});
            if !path.is_empty() { settings["path"] = json!(path); }
            if !host.is_empty() { settings["headers"] = json!({"Host":host}); }
            result.insert("wsSettings".to_owned(), settings);
        }
        "grpc" => {
            let mut settings = json!({});
            if !service_name.is_empty() { settings["serviceName"] = json!(service_name); }
            if !host.is_empty() { settings["authority"] = json!(host); }
            if mode == "multi" { settings["multiMode"] = json!(true); }
            result.insert("grpcSettings".to_owned(), settings);
        }
        "xhttp" => {
            let mut settings = json!({});
            if !host.is_empty() { settings["host"] = json!(host); }
            if !path.is_empty() { settings["path"] = json!(path); }
            if !mode.is_empty() { settings["mode"] = json!(mode); }
            result.insert("xhttpSettings".to_owned(), settings);
        }
        _ => {}
    }
    Value::Object(result)
}

fn make_tag(fragment: &str, host: &str, port: u16) -> String {
    let encoded = format!("name={fragment}");
    let decoded = url::form_urlencoded::parse(encoded.as_bytes())
        .next().map(|(_, value)| value.into_owned()).unwrap_or_default();
    let cleaned: String = decoded.chars()
        .filter(|c| c.is_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.' ))
        .collect();
    let cleaned = cleaned.trim();
    if cleaned.is_empty() { format!("{host}-{port}") } else { cleaned.to_owned() }
}

fn default<'a>(value: &'a str, fallback: &'a str) -> &'a str {
    if value.is_empty() { fallback } else { value }
}

fn string_field(value: &Value, key: &str) -> String {
    value.get(key).map(|v| match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => String::new(),
    }).unwrap_or_default()
}

fn number_field(value: &Value, key: &str) -> Option<u16> {
    value.get(key).and_then(|v| {
        v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse::<u64>().ok()))
    }).and_then(|n| u16::try_from(n).ok())
}

fn deduplicate_tags(outbounds: &mut [Value]) {
    let mut used = std::collections::HashSet::new();
    for outbound in outbounds {
        let original = outbound.get("tag").and_then(Value::as_str).unwrap_or("node").to_owned();
        let mut tag = original.clone();
        let mut suffix = 1usize;
        while !used.insert(tag.clone()) {
            tag = format!("{original}--{suffix}");
            suffix += 1;
        }
        outbound["tag"] = json!(tag);
    }
}

#[cfg(test)]
mod tests {
    use super::parse_subscription;
    use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
    use base64::Engine;
    use serde_json::json;

    #[test]
    fn parses_vless_reality_and_xhttp() {
        let parsed = parse_subscription(
            b"vless://uuid@example.com:443?type=xhttp&security=reality&sni=site.test&pbk=pub&sid=01&spx=%2F#My%20Node"
        ).unwrap();
        assert_eq!(parsed.outbounds.len(), 1);
        assert_eq!(parsed.outbounds[0]["protocol"], "vless");
        assert_eq!(parsed.outbounds[0]["tag"], "My Node");
        assert_eq!(parsed.outbounds[0]["streamSettings"]["security"], "reality");
        assert_eq!(parsed.outbounds[0]["streamSettings"]["xhttpSettings"]["path"], "");
    }

    #[test]
    fn parses_vmess_url_safe_base64_without_padding() {
        let vmess = json!({"add":"vmess.example","port":"8443","id":"uuid","aid":"0","net":"ws","tls":"tls","host":"cdn.test","path":"/ws"});
        let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&vmess).unwrap());
        let parsed = parse_subscription(format!("vmess://{payload}#VMess").as_bytes()).unwrap();
        assert_eq!(parsed.outbounds[0]["protocol"], "vmess");
        assert_eq!(parsed.outbounds[0]["settings"]["vnext"][0]["port"], 8443);
        assert_eq!(parsed.outbounds[0]["streamSettings"]["wsSettings"]["path"], "/ws");
    }

    #[test]
    fn parses_trojan_shadowsocks_and_hysteria2() {
        let ss = STANDARD.encode("aes-128-gcm:secret");
        let body = format!(
            "trojan://pass@trojan.example:443?sni=example.com#T\nss://{ss}@ss.example:8388#S\nhysteria2://auth@hy.example:443?sni=hy.example#H"
        );
        let parsed = parse_subscription(body.as_bytes()).unwrap();
        assert_eq!(parsed.outbounds.len(), 3);
        assert_eq!(parsed.outbounds[0]["protocol"], "trojan");
        assert_eq!(parsed.outbounds[1]["protocol"], "shadowsocks");
        assert_eq!(parsed.outbounds[2]["protocol"], "hysteria");
    }

    #[test]
    fn decodes_base64_encoded_subscription_body() {
        let links = b"vless://uuid@example.com:443#Node";
        let encoded = STANDARD.encode(links);
        let parsed = parse_subscription(encoded.as_bytes()).unwrap();
        assert_eq!(parsed.outbounds.len(), 1);
    }

    #[test]
    fn decodes_url_safe_base64_subscription_body() {
        let links = b"vless://uuid@example.com:443#Node";
        let encoded = URL_SAFE_NO_PAD.encode(links);
        let parsed = parse_subscription(encoded.as_bytes()).unwrap();
        assert_eq!(parsed.outbounds.len(), 1);
        assert_eq!(parsed.outbounds[0]["tag"], "Node");
    }

    #[test]
    fn preserves_json_outbound_documents() {
        let json = br#"{"outbounds":[{"tag":"node","protocol":"freedom","custom":{"x":1}}]}"#;
        let parsed = parse_subscription(json).unwrap();
        assert_eq!(parsed.outbounds[0]["custom"]["x"], 1);
    }

    #[test]
    fn partial_parse_keeps_supported_entries_and_reports_failures() {
        let body = b"vless://uuid@example.com:443#Good\nunsupported://bad#Bad";
        let parsed = parse_subscription(body).unwrap();
        assert_eq!(parsed.outbounds.len(), 1);
        assert_eq!(parsed.unrecognized, 1);
        assert_eq!(parsed.errors.len(), 1);
    }
}
