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

    fn read(mut res: ureq::http::Response<ureq::Body>) -> Result<serde_json::Value, ApiCallError> {
        let status = res.status().as_u16();
        let value: serde_json::Value = res
            .body_mut()
            .read_json()
            .map_err(|e| ApiCallError::Transport(format!("bad response body: {e}")))?;
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
