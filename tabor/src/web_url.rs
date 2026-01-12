pub fn normalize_web_url(input: &str) -> String {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    if trimmed.contains("://")
        || trimmed.starts_with("about:")
        || trimmed.starts_with("file:")
        || trimmed.starts_with("data:")
    {
        return trimmed.to_string();
    }

    let scheme = if is_local_host(trimmed) { "http" } else { "https" };
    format!("{scheme}://{trimmed}")
}

fn is_local_host(input: &str) -> bool {
    let end = input.find(|c| matches!(c, '/' | '?' | '#')).unwrap_or(input.len());
    let mut host = &input[..end];

    if let Some((_, tail)) = host.rsplit_once('@') {
        host = tail;
    }

    if host.starts_with('[') {
        if let Some(close) = host.find(']') {
            host = &host[1..close];
        }
    } else if let Some((name, port)) = host.rsplit_once(':') {
        if !name.contains(':') && !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) {
            host = name;
        }
    }

    let host = host.to_ascii_lowercase();
    if host == "localhost" || host == "0.0.0.0" || host == "::1" {
        return true;
    }

    if host == "127.0.0.1" {
        return true;
    }

    host.bytes().all(|b| b.is_ascii_digit() || b == b'.') && host.starts_with("127.")
}
