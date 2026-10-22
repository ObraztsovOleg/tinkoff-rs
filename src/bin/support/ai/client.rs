//! Диалог с моделью: Anthropic Messages и OpenAI-совместимый Chat Completions.
use std::time::Duration as StdDuration;

use secrecy::{ExposeSecret, SecretString};
use serde_json::{Value, json};
use tinkoff_rs::{Error, Result};

use super::{
    profiles::{Profile, Provider},
    usage::Meter,
};

/// Вызов инструмента, запрошенный моделью.
#[derive(Clone, Debug)]
pub(crate) struct ToolCall {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) input: Value,
}

/// Ответ модели за один шаг: текст и вызовы инструментов.
pub(crate) struct Turn {
    pub(crate) text: String,
    pub(crate) calls: Vec<ToolCall>,
    /// Anthropic приостановил долгий серверный инструмент: шаг нужно продолжить.
    pub(crate) paused: bool,
}

/// Диалог в формате выбранного провайдера.
pub(crate) struct Chat<'a> {
    pub(crate) name: String,
    pub(crate) profile: &'a Profile,
    pub(crate) token: Option<SecretString>,
    pub(crate) client: reqwest::Client,
    pub(crate) system: String,
    pub(crate) messages: Vec<Value>,
    /// Счётчик токенов работы, к которой относится диалог.
    pub(crate) meter: Meter,
}

/// Диалог с отметкой кэша на последнем сообщении пользователя.
fn cached_messages(messages: &[Value]) -> Value {
    let mut messages = messages.to_vec();
    if let Some(last) = messages.last_mut().filter(|m| m["role"] == "user") {
        if let Some(text) = last["content"].as_str() {
            last["content"] = json!([{"type": "text", "text": text}]);
        }
        if let Some(block) = last["content"]
            .as_array_mut()
            .and_then(|blocks| blocks.last_mut())
        {
            block["cache_control"] = json!({"type": "ephemeral"});
        }
    }
    json!(messages)
}

pub(crate) fn model_error(message: impl Into<String>) -> Error {
    Error::Strategy {
        strategy: "ai-model".to_owned(),
        message: message.into(),
    }
}

impl<'a> Chat<'a> {
    pub(crate) fn new(
        name: String,
        profile: &'a Profile,
        system: String,
        meter: Meter,
    ) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(StdDuration::from_secs(profile.timeout_seconds))
            .user_agent("tinkoff-rs-ai/0.3")
            .build()
            .map_err(|error| model_error(format!("cannot create HTTP client: {error}")))?;
        let token = profile.token()?;
        // Облачный Anthropic без ключа всё равно ответит 401: понятная ошибка сразу.
        if token.is_none() && profile.provider == Provider::Anthropic {
            return Err(Error::Configuration(format!(
                "AI profile {name} has no token: set token in the AI model profiles file"
            )));
        }
        if let Ok(mut spend) = meter.lock() {
            spend.profile = Some(name.clone());
            spend.model = Some(profile.model.clone());
            spend.prices = profile.effective_prices();
        }
        Ok(Self {
            name,
            profile,
            token,
            client,
            system,
            messages: Vec::new(),
            meter,
        })
    }

    pub(crate) fn label(&self) -> String {
        format!("{} ({})", self.name, self.profile.model)
    }

    pub(crate) fn user(&mut self, text: &str) {
        self.messages.push(json!({"role": "user", "content": text}));
    }

    /// Один запрос к модели. `force` заставляет вызвать указанный инструмент.
    pub(crate) async fn step(&mut self, tools: &[Value], force: Option<&str>) -> Result<Turn> {
        let base = self.profile.base_url();
        let (url, body) = match self.profile.provider {
            Provider::Anthropic => {
                let mut tools: Vec<Value> = tools
                    .iter()
                    .map(|tool| json!({"name": tool["name"], "description": tool["description"], "input_schema": tool["parameters"]}))
                    .collect();
                if self.profile.web_search && force.is_none() {
                    tools.push(
                        json!({"type": "web_search_20250305", "name": "web_search", "max_uses": 5}),
                    );
                }
                // Кэш промпта: инструменты и системный промпт, затем весь диалог до
                // последнего сообщения. Следующий шаг читает этот префикс из кэша.
                let (system, messages) = if self.profile.prompt_cache {
                    (
                        json!([{"type": "text", "text": self.system, "cache_control": {"type": "ephemeral"}}]),
                        cached_messages(&self.messages),
                    )
                } else {
                    (json!(self.system), json!(self.messages))
                };
                let mut body = json!({
                    "model": self.profile.model, "max_tokens": self.profile.max_output_tokens,
                    "system": system, "messages": messages, "tools": tools,
                });
                if let Some(name) = force {
                    body["tool_choice"] = json!({"type": "tool", "name": name});
                }
                if let Some(temperature) = self.profile.temperature {
                    body["temperature"] = json!(temperature);
                }
                (format!("{base}/messages"), body)
            }
            Provider::Openai => {
                let mut messages = vec![json!({"role": "system", "content": self.system})];
                messages.extend(self.messages.iter().cloned());
                let tools: Vec<Value> = tools
                    .iter()
                    .map(|tool| json!({"type": "function", "function": tool}))
                    .collect();
                // Официальный OpenAI требует max_completion_tokens; совместимые серверы — max_tokens.
                let limit = if base.contains("api.openai.com") {
                    "max_completion_tokens"
                } else {
                    "max_tokens"
                };
                let mut body =
                    json!({"model": self.profile.model, "messages": messages, "tools": tools});
                body[limit] = json!(self.profile.max_output_tokens);
                body["tool_choice"] = force.map_or_else(
                    || json!("auto"),
                    |name| json!({"type": "function", "function": {"name": name}}),
                );
                if let Some(temperature) = self.profile.temperature {
                    body["temperature"] = json!(temperature);
                }
                (format!("{base}/chat/completions"), body)
            }
        };
        let mut request = self.client.post(&url).json(&body);
        for (name, value) in &self.profile.headers {
            request = request.header(name, value);
        }
        request = match (self.profile.provider, &self.token) {
            (Provider::Anthropic, token) => {
                let request = request.header("anthropic-version", "2023-06-01");
                match token {
                    Some(token) => request.header("x-api-key", token.expose_secret()),
                    None => request,
                }
            }
            (Provider::Openai, Some(token)) => request.bearer_auth(token.expose_secret()),
            (Provider::Openai, None) => request,
        };
        let response = request.send().await.map_err(|error| {
            model_error(format!(
                "{}: request failed: {}",
                self.label(),
                error.without_url()
            ))
        })?;
        let status = response.status();
        let text = response.text().await.map_err(|error| {
            model_error(format!(
                "{}: cannot read response: {}",
                self.label(),
                error.without_url()
            ))
        })?;
        if !status.is_success() {
            let detail: String = text.chars().take(400).collect();
            return Err(model_error(format!(
                "{}: HTTP {status}: {detail}",
                self.label()
            )));
        }
        let value: Value = serde_json::from_str(&text).map_err(|error| {
            model_error(format!("{}: invalid JSON response: {error}", self.label()))
        })?;
        if let Ok(mut spend) = self.meter.lock() {
            match self.profile.provider {
                Provider::Anthropic => spend.usage.add_anthropic(&value["usage"]),
                Provider::Openai => spend.usage.add_openai(&value["usage"]),
            }
        }
        match self.profile.provider {
            Provider::Anthropic => Ok(self.anthropic_turn(&value)),
            Provider::Openai => self.openai_turn(&value),
        }
    }

    pub(crate) fn anthropic_turn(&mut self, value: &Value) -> Turn {
        let content = value["content"].as_array().cloned().unwrap_or_default();
        let mut turn = Turn {
            text: String::new(),
            calls: Vec::new(),
            paused: value["stop_reason"] == "pause_turn",
        };
        for block in &content {
            match block["type"].as_str() {
                Some("text") => turn
                    .text
                    .push_str(block["text"].as_str().unwrap_or_default()),
                Some("tool_use") => turn.calls.push(ToolCall {
                    id: block["id"].as_str().unwrap_or_default().to_owned(),
                    name: block["name"].as_str().unwrap_or_default().to_owned(),
                    input: block["input"].clone(),
                }),
                _ => {}
            }
        }
        self.messages
            .push(json!({"role": "assistant", "content": content}));
        turn
    }

    pub(crate) fn openai_turn(&mut self, value: &Value) -> Result<Turn> {
        let message = value["choices"][0]["message"].clone();
        if message.is_null() {
            return Err(model_error(format!(
                "{}: response has no message",
                self.label()
            )));
        }
        let mut turn = Turn {
            text: message["content"].as_str().unwrap_or_default().to_owned(),
            calls: Vec::new(),
            paused: false,
        };
        for call in message["tool_calls"].as_array().into_iter().flatten() {
            let arguments = call["function"]["arguments"].as_str().unwrap_or("{}");
            turn.calls.push(ToolCall {
                id: call["id"].as_str().unwrap_or_default().to_owned(),
                name: call["function"]["name"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
                input: serde_json::from_str(arguments)
                    .unwrap_or_else(|_| json!({"_invalid_json": arguments})),
            });
        }
        let mut stored = json!({"role": "assistant", "content": message["content"]});
        if message["tool_calls"].is_array() {
            stored["tool_calls"] = message["tool_calls"].clone();
        }
        self.messages.push(stored);
        Ok(turn)
    }

    /// Возвращает результаты инструментов модели в формате провайдера.
    pub(crate) fn tool_results(&mut self, results: Vec<(String, String)>, note: Option<&str>) {
        match self.profile.provider {
            Provider::Anthropic => {
                let mut content: Vec<Value> = results
                    .into_iter()
                    .map(|(id, text)| json!({"type": "tool_result", "tool_use_id": id, "content": text}))
                    .collect();
                if let Some(note) = note {
                    content.push(json!({"type": "text", "text": note}));
                }
                self.messages
                    .push(json!({"role": "user", "content": content}));
            }
            Provider::Openai => {
                for (id, text) in results {
                    self.messages
                        .push(json!({"role": "tool", "tool_call_id": id, "content": text}));
                }
                if let Some(note) = note {
                    self.user(note);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::{
        decision::json_in_text,
        testkit::{profile, provider},
        tools::tools,
        usage::{meter, snapshot},
    };

    #[tokio::test]
    async fn anthropic_protocol_calls_tools_and_returns_results() {
        let (url, seen) = provider(vec![
            json!({"content": [{"type": "text", "text": "Смотрю новости"}, {"type": "tool_use", "id": "t1", "name": "get_news", "input": {"query": "Сбербанк"}}], "stop_reason": "tool_use",
                   "usage": {"input_tokens": 2500, "cache_creation_input_tokens": 3000, "output_tokens": 80}}),
            json!({"content": [{"type": "tool_use", "id": "t2", "name": "submit_decision", "input": {"weights": [{"ticker": "SBER", "weight_percent": 20}], "rationale": "рост"}}], "stop_reason": "tool_use",
                   "usage": {"input_tokens": 300, "cache_read_input_tokens": 5500, "output_tokens": 120}}),
        ]).await;
        let mut profile = profile(Provider::Anthropic, &url);
        profile.prices =
            toml::from_str("input = 3\noutput = 15\ncache_write = 3.75\ncache_read = 0.3").ok();
        let counter = meter();
        let mut chat =
            Chat::new("claude".into(), &profile, "system".into(), counter.clone()).unwrap();
        chat.user("решение");
        let turn = chat.step(&tools(true), None).await.unwrap();
        assert_eq!(
            (
                turn.text.as_str(),
                turn.calls[0].name.as_str(),
                turn.calls[0].input["query"].as_str()
            ),
            ("Смотрю новости", "get_news", Some("Сбербанк"))
        );
        chat.tool_results(vec![("t1".into(), "{\"count\":0}".into())], Some("лимит"));
        let turn = chat
            .step(&tools(true), Some("submit_decision"))
            .await
            .unwrap();
        assert_eq!(turn.calls[0].input["weights"][0]["ticker"], "SBER");
        let seen = seen.lock().unwrap();
        let (head, first) = &seen[0];
        assert!(
            head.contains("x-api-key: test-key")
                && head.contains("anthropic-version: 2023-06-01")
                && head.starts_with("post /v1/messages")
        );
        // The cached prefix: tools and system prompt, then the dialogue up to the last message.
        assert_eq!(
            (first["system"][0]["text"].as_str(), first["model"].as_str()),
            (Some("system"), Some("test-model"))
        );
        assert_eq!(first["system"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(
            first["messages"][0]["content"][0],
            json!({"type": "text", "text": "решение", "cache_control": {"type": "ephemeral"}})
        );
        assert!(first["tools"][0]["input_schema"].is_object());
        let second = &seen[1].1;
        assert_eq!(
            second["messages"][2]["content"][1]["cache_control"]["type"],
            "ephemeral"
        );
        assert!(
            second["messages"][0]["content"][0]["cache_control"].is_null(),
            "only the newest message carries the cache mark"
        );
        assert_eq!(
            chat.messages[0]["content"], "решение",
            "the stored dialogue stays unmarked"
        );
        let spend = snapshot(&counter);
        assert_eq!(
            (
                spend.usage.requests,
                spend.usage.input_tokens,
                spend.usage.cache_read_tokens
            ),
            (2, 2800, 5500)
        );
        // 2800×3 + 200×15 + 3000×3.75 + 5500×0.3 per million.
        assert!((spend.cost().unwrap() - 0.0243).abs() < 1e-9);
        assert_eq!(
            second["tool_choice"],
            json!({"type": "tool", "name": "submit_decision"})
        );
        let reply = &second["messages"][2]["content"];
        assert_eq!(
            (
                reply[0]["type"].as_str(),
                reply[0]["tool_use_id"].as_str(),
                reply[1]["text"].as_str()
            ),
            (Some("tool_result"), Some("t1"), Some("лимит"))
        );
    }

    #[tokio::test]
    async fn openai_compatible_protocol_calls_tools_and_returns_results() {
        let (url, seen) = provider(vec![
            json!({"choices": [{"message": {"role": "assistant", "content": null, "tool_calls": [{"id": "c1", "type": "function", "function": {"name": "get_indicators", "arguments": "{\"tickers\":[\"SBER\"],\"indicators\":[\"rsi_14\"]}"}}]}}]}),
            json!({"choices": [{"message": {"role": "assistant", "content": "{\"weights\":[{\"ticker\":\"SBER\",\"weight_percent\":10}],\"rationale\":\"x\"}"}}]}),
        ]).await;
        let profile = profile(Provider::Openai, &url);
        let mut chat = Chat::new("local".into(), &profile, "system".into(), meter()).unwrap();
        chat.user("решение");
        let turn = chat.step(&tools(true), None).await.unwrap();
        assert_eq!(
            (
                turn.calls[0].id.as_str(),
                turn.calls[0].input["indicators"][0].as_str()
            ),
            ("c1", Some("rsi_14"))
        );
        chat.tool_results(vec![("c1".into(), "{}".into())], None);
        let turn = chat.step(&tools(true), None).await.unwrap();
        assert_eq!(
            json_in_text(&turn.text).unwrap()["weights"][0]["weight_percent"],
            10
        );
        let seen = seen.lock().unwrap();
        let (head, first) = &seen[0];
        assert!(
            head.contains("authorization: bearer test-key")
                && head.starts_with("post /v1/chat/completions")
        );
        assert_eq!(
            (
                first["messages"][0]["role"].as_str(),
                first["tools"][0]["type"].as_str(),
                first["max_tokens"].as_u64()
            ),
            (Some("system"), Some("function"), Some(4096))
        );
        let messages = &seen[1].1["messages"];
        assert_eq!(
            (
                messages[2]["tool_calls"][0]["id"].as_str(),
                messages[3]["role"].as_str(),
                messages[3]["tool_call_id"].as_str()
            ),
            (Some("c1"), Some("tool"), Some("c1"))
        );
    }
}
