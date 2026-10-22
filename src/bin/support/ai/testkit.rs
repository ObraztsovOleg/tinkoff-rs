//! Общие заготовки тестов: поддельный HTTP-провайдер модели.
use serde_json::{Value, json};

use super::profiles::{Profile, Provider};

/// Поддельный провайдер: отдаёт ответы по очереди и запоминает запросы.
pub(crate) async fn provider(
    responses: Vec<Value>,
) -> (
    String,
    std::sync::Arc<std::sync::Mutex<Vec<(String, Value)>>>,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let log = seen.clone();
    tokio::spawn(async move {
        for response in responses {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = Vec::new();
            let mut chunk = [0_u8; 4096];
            let (head, body) = loop {
                let n = socket.read(&mut chunk).await.unwrap();
                buffer.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&buffer).into_owned();
                if let Some(split) = text.find("\r\n\r\n") {
                    let head = text[..split].to_lowercase();
                    let length: usize = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .map_or(0, |v| v.trim().parse().unwrap());
                    if buffer.len() >= split + 4 + length {
                        break (
                            head,
                            serde_json::from_slice(&buffer[split + 4..split + 4 + length]).unwrap(),
                        );
                    }
                }
            };
            log.lock().unwrap().push((head, body));
            let payload = response.to_string();
            let reply = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{payload}",
                payload.len()
            );
            socket.write_all(reply.as_bytes()).await.unwrap();
        }
    });
    (format!("http://{address}/v1"), seen)
}

pub(crate) fn profile(provider: Provider, url: &str) -> Profile {
    toml::from_str::<Profile>(&format!(
        "provider = \"{}\"\nurl = \"{url}\"\nmodel = \"test-model\"\ntoken = \"test-key\"",
        json!(provider).as_str().unwrap()
    ))
    .unwrap()
}
