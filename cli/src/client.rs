//! Blocking HTTP client over ureq 3 (no TLS stack — loopback only). Non-2xx responses
//! surface the server's LLM-audience `error.message` verbatim.

use std::time::Duration;

use coretempo_core::types::ApiErrorBody;

use crate::connect::Connection;

#[derive(Debug)]
pub enum ApiCallError {
    /// Server answered with `{error:{code,message}}` — message is printed verbatim.
    Api {
        #[expect(dead_code, reason = "machine-readable code kept for Debug output")]
        code: String,
        message: String,
    },
    Transport(String),
}

impl std::fmt::Display for ApiCallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiCallError::Api { message, .. } => write!(f, "{message}"),
            ApiCallError::Transport(message) => write!(
                f,
                "cannot reach the CoreTempo server: {message}; is a run active?"
            ),
        }
    }
}

impl std::error::Error for ApiCallError {}

pub struct Client {
    agent: ureq::Agent,
    base: String,
    token: String,
    pub agent_id: Option<String>,
}

impl Client {
    pub fn new(conn: &Connection) -> Client {
        // 330 s global timeout: must outlive the 300 s ?wait cap plus slack.
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(330)))
            .build();
        Client {
            agent: ureq::Agent::new_with_config(config),
            base: format!("http://127.0.0.1:{}/v1", conn.port),
            token: conn.token.clone(),
            agent_id: conn.agent_id.clone(),
        }
    }

    /// A client with no global timeout: `attach` holds SSE streams open for as
    /// long as the terminal is attached.
    pub fn new_untimed(conn: &Connection) -> Client {
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(None)
            .build();
        Client {
            agent: ureq::Agent::new_with_config(config),
            base: format!("http://127.0.0.1:{}/v1", conn.port),
            token: conn.token.clone(),
            agent_id: conn.agent_id.clone(),
        }
    }

    pub fn get(&self, path: &str) -> Result<serde_json::Value, ApiCallError> {
        let res = self
            .agent
            .get(format!("{}{path}", self.base))
            .header("Authorization", format!("Bearer {}", self.token))
            .call()
            .map_err(|e| ApiCallError::Transport(e.to_string()))?;
        Client::read(res)
    }

    pub fn post(
        &self,
        path: &str,
        body: &serde_json::Value,
    ) -> Result<serde_json::Value, ApiCallError> {
        let mut req = self
            .agent
            .post(format!("{}{path}", self.base))
            .header("Authorization", format!("Bearer {}", self.token));
        if let Some(id) = &self.agent_id {
            req = req.header("X-CoreTempo-Agent", id);
        }
        let res = req
            .send_json(body)
            .map_err(|e| ApiCallError::Transport(e.to_string()))?;
        Client::read(res)
    }

    pub fn delete(&self, path: &str) -> Result<serde_json::Value, ApiCallError> {
        let res = self
            .agent
            .delete(format!("{}{path}", self.base))
            .header("Authorization", format!("Bearer {}", self.token))
            .call()
            .map_err(|e| ApiCallError::Transport(e.to_string()))?;
        Client::read(res)
    }

    /// Raw bytes to a PTY write route; the server answers 204.
    pub fn post_raw(&self, path: &str, bytes: &[u8]) -> Result<(), ApiCallError> {
        let res = self
            .agent
            .post(format!("{}{path}", self.base))
            .header("Authorization", format!("Bearer {}", self.token))
            .header("Content-Type", "application/octet-stream")
            .send(bytes)
            .map_err(|e| ApiCallError::Transport(e.to_string()))?;
        Client::read(res).map(|_| ())
    }

    /// An SSE stream's body, read line by line by the caller.
    pub fn stream(&self, path: &str) -> Result<Box<dyn std::io::Read + Send>, ApiCallError> {
        let res = self
            .agent
            .get(format!("{}{path}", self.base))
            .header("Authorization", format!("Bearer {}", self.token))
            .call()
            .map_err(|e| ApiCallError::Transport(e.to_string()))?;
        let status = res.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(Client::read(res)
                .err()
                .unwrap_or_else(|| ApiCallError::Transport(format!("HTTP {status}"))));
        }
        Ok(Box::new(res.into_body().into_reader()))
    }

    /// A 204 (the delete and PTY-write routes) carries no body; every other
    /// response is JSON.
    fn read(mut res: ureq::http::Response<ureq::Body>) -> Result<serde_json::Value, ApiCallError> {
        let status = res.status().as_u16();
        let text = res
            .body_mut()
            .read_to_string()
            .map_err(|e| ApiCallError::Transport(format!("bad response body: {e}")))?;
        let value: serde_json::Value = if text.trim().is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_str(&text)
                .map_err(|e| ApiCallError::Transport(format!("bad response body: {e}")))?
        };
        if (200..300).contains(&status) {
            return Ok(value);
        }
        match serde_json::from_value::<ApiErrorBody>(value.clone()) {
            Ok(body) => Err(ApiCallError::Api {
                code: body.error.code,
                message: body.error.message,
            }),
            Err(_) => Err(ApiCallError::Api {
                code: "internal".to_string(),
                message: format!("HTTP {status}: {value}"),
            }),
        }
    }
}
