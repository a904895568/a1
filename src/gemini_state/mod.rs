use std::sync::LazyLock;

use axum::{
    body::Body,
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use colored::Colorize;
use futures::{Stream, future::Either, stream};
use hyper_util::client::legacy::connect::HttpConnector;
use serde::Serialize;
use serde_json::Value;
use snafu::ResultExt;
use strum::Display;
use tokio::spawn;
use tracing::{Instrument, Level, error, info, span, warn};
use wreq::{Client, ClientBuilder, header::AUTHORIZATION};
use yup_oauth2::{CustomHyperClientBuilder, ServiceAccountAuthenticator, ServiceAccountKey};

use crate::{
    config::{CLEWDR_CONFIG, GEMINI_ENDPOINT, KeyStatus},
    error::{CheckGeminiErr, ClewdrError, InvalidUriSnafu, RquestSnafu},
    gemini_body::GeminiArgs,
    middleware::gemini::GeminiContext,
    services::{
        cache::{CACHE, GetHashKey},
        key_manager::KeyEventSender,
    },
    types::gemini::response::{FinishReason, GeminiResponse},
};

#[derive(Clone, Display, PartialEq, Eq)]
pub enum GeminiApiFormat {
    Gemini,
    OpenAI,
}

static DUMMY_CLIENT: LazyLock<Client> = LazyLock::new(Client::new);

// TODO: replace yup-oauth2 with oauth2 crate
async fn get_token(sa_key: ServiceAccountKey) -> Result<String, ClewdrError> {
    const SCOPES: [&str; 1] = ["https://www.googleapis.com/auth/cloud-platform"];
    let token = if let Some(proxy) = CLEWDR_CONFIG.load().proxy.to_owned() {
        let proxy = proxy
            .trim_start_matches("http://")
            .trim_start_matches("https://")
            .trim_start_matches("socks5://");
        let proxy = format!("http://{proxy}");
        let proxy_uri = proxy.parse().context(InvalidUriSnafu {
            uri: proxy.to_owned(),
        })?;
        let proxy = hyper_http_proxy::Proxy::new(hyper_http_proxy::Intercept::All, proxy_uri);
        let connector = HttpConnector::new();
        let proxy_connector = hyper_http_proxy::ProxyConnector::from_proxy(connector, proxy)?;
        let client =
            hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
                .pool_max_idle_per_host(0)
                .build(proxy_connector);
        let client_builder = CustomHyperClientBuilder::from(client);
        let auth = ServiceAccountAuthenticator::with_client(sa_key, client_builder)
            .build()
            .await?;
        auth.token(&SCOPES).await?
    } else {
        let auth = ServiceAccountAuthenticator::builder(sa_key).build().await?;
        auth.token(&SCOPES).await?
    };
    let token = token.token().ok_or(ClewdrError::UnexpectedNone {
        msg: "Oauth token is None",
    })?;
    Ok(token.into())
}

#[derive(Clone)]
pub struct GeminiState {
    pub model: String,
    pub vertex: bool,
    pub path: String,
    pub key: Option<KeyStatus>,
    pub stream: bool,
    pub query: GeminiArgs,
    pub event_sender: KeyEventSender,
    pub api_format: GeminiApiFormat,
    pub client: Client,
    pub cache_key: Option<(u64, usize)>,
}

impl GeminiState {
    /// Create a new AppState instance
    pub fn new(tx: KeyEventSender) -> Self {
        GeminiState {
            model: String::new(),
            vertex: false,
            path: String::new(),
            query: GeminiArgs::default(),
            stream: false,
            key: None,
            event_sender: tx,
            api_format: GeminiApiFormat::Gemini,
            client: DUMMY_CLIENT.to_owned(),
            cache_key: None,
        }
    }

    pub async fn report_403(&self) -> Result<(), ClewdrError> {
        if let Some(mut key) = self.key.to_owned() {
            key.count_403 += 1;
            self.event_sender.return_key(key).await?;
        }
        Ok(())
    }

    pub async fn request_key(&mut self) -> Result<(), ClewdrError> {
        let key = self.event_sender.request().await?;
        self.key = Some(key.to_owned());
        let client = ClientBuilder::new();
        let client = if let Some(proxy) = CLEWDR_CONFIG.load().proxy.to_owned() {
            client.proxy(proxy)
        } else {
            client
        };
        self.client = client.build().context(RquestSnafu {
            msg: "Failed to build Gemini client",
        })?;
        Ok(())
    }

    pub fn update_from_ctx(&mut self, ctx: &GeminiContext) {
        self.path = ctx.path.to_owned();
        self.stream = ctx.stream.to_owned();
        self.query = ctx.query.to_owned();
        self.model = ctx.model.to_owned();
        self.vertex = ctx.vertex.to_owned();
        self.api_format = ctx.api_format.to_owned();
    }

    async fn vertex_response(
        &mut self,
        p: impl Sized + Serialize,
    ) -> Result<wreq::Response, ClewdrError> {
        let client = ClientBuilder::new();
        let client = if let Some(proxy) = CLEWDR_CONFIG.load().proxy.to_owned() {
            client.proxy(proxy)
        } else {
            client
        };
        self.client = client.build().context(RquestSnafu {
            msg: "Failed to build Gemini client",
        })?;
        let method = if self.stream {
            "streamGenerateContent"
        } else {
            "generateContent"
        };

        // Get an access token
        let Some(cred) = CLEWDR_CONFIG.load().vertex.credential.to_owned() else {
            return Err(ClewdrError::BadRequest {
                msg: "Vertex credential not found",
            });
        };

        let access_token = get_token(cred.to_owned()).await?;
        let bearer = format!("Bearer {access_token}");
        let res = match self.api_format {
            GeminiApiFormat::Gemini => {
                let endpoint = format!(
                    "https://aiplatform.googleapis.com/v1/projects/{}/locations/global/publishers/google/models/{}:{method}",
                    cred.project_id.unwrap_or_default(),
                    self.model
                );
                let query_vec = self.query.to_vec();
                self
                    .client
                    .post(endpoint)
                    .query(&query_vec)
                    .header(AUTHORIZATION, bearer)
                    .json(&p)
                    .send()
                    .await
                    .context(RquestSnafu {
                        msg: "Failed to send request to Gemini Vertex API",
                    })?
            }
            GeminiApiFormat::OpenAI => {
                self.client
                    .post(format!(
                        "https://aiplatform.googleapis.com/v1beta1/projects/{}/locations/global/endpoints/openapi/chat/completions",
                        cred.project_id.unwrap_or_default(),
                    ))
                    .header(AUTHORIZATION, bearer)
                    .json(&p)
                    .send()
                    .await
                    .context(RquestSnafu {
                        msg: "Failed to send request to Gemini Vertex OpenAI API",
                    })?
            }
        };
        let res = res.check_gemini().await?;
        Ok(res)
    }

    pub async fn send_chat(
        &mut self,
        p: impl Sized + Serialize,
    ) -> Result<wreq::Response, ClewdrError> {
        if self.vertex {
            let res = self.vertex_response(p).await?;
            return Ok(res);
        }
        self.request_key().await?;
        let Some(key) = self.key.to_owned() else {
            return Err(ClewdrError::UnexpectedNone {
                msg: "Key is None, did you request a key?",
            });
        };
        info!("[KEY] {}", key.key.ellipse().green());
        let key = key.key.to_string();
        let res = match self.api_format {
            GeminiApiFormat::Gemini => {
                let mut query_vec = self.query.to_vec();
                query_vec.push(("key", key.as_str()));
                self.client
                    .post(format!("{}/v1beta/{}", GEMINI_ENDPOINT, self.path))
                    .query(&query_vec)
                    .json(&p)
                    .send()
                    .await
                    .context(RquestSnafu {
                        msg: "Failed to send request to Gemini API",
                    })?
            }
            GeminiApiFormat::OpenAI => self
                .client
                .post(format!("{GEMINI_ENDPOINT}/v1beta/openai/chat/completions",))
                .header(AUTHORIZATION, format!("Bearer {key}"))
                .json(&p)
                .send()
                .await
                .context(RquestSnafu {
                    msg: "Failed to send request to Gemini OpenAI API",
                })?,
        };
        let res = res.check_gemini().await?;
        Ok(res)
    }

    pub async fn try_chat(
        &mut self,
        p: impl Serialize + GetHashKey + Clone,
    ) -> Result<Response, ClewdrError> {
        let mut err = None;
        for i in 0..CLEWDR_CONFIG.load().max_retries + 1 {
            if i > 0 {
                info!("[RETRY] attempt: {}", i.to_string().green());
            }
            let mut state = self.to_owned();
            let p = p.to_owned();

            match state.send_chat(p).await {
                Ok(resp) => {
                    let Ok(stream) = state.check_empty_choices(resp).await else {
                        warn!("Empty choices");
                        err = Some(ClewdrError::EmptyChoices);
                        continue;
                    };
                    let res = transform_response(self.cache_key, stream).await;
                    return Ok(res);
                }
                Err(e) => {
                    if let Some(key) = state.key.to_owned() {
                        error!("[{}] {}", key.key.ellipse().green(), e);
                    } else {
                        error!("{}", e);
                    }
                    match e {
                        ClewdrError::GeminiHttpError { code, .. } => {
                            if code == 403 {
                                spawn(async move {
                                    state.report_403().await.unwrap_or_else(|e| {
                                        error!("Failed to report 403: {}", e);
                                    });
                                });
                            }
                            err = Some(e);
                            continue;
                        }
                        e => return Err(e),
                    }
                }
            }
        }
        error!("Max retries exceeded");
        if let Some(e) = err {
            return Err(e);
        }
        Err(ClewdrError::TooManyRetries)
    }

    pub async fn try_from_cache(
        &self,
        p: &(impl Serialize + GetHashKey + Clone + Send + 'static),
    ) -> Option<axum::response::Response> {
        let key = p.get_hash();
        if let Some(stream) = CACHE.pop(key).await {
            info!("Found response for key: {}", key);
            return Some(Body::from_stream(stream).into_response());
        }
        for id in 0..CLEWDR_CONFIG.load().cache_response {
            let mut state = self.to_owned();
            state.cache_key = Some((key, id));
            let p = p.to_owned();
            let cache_span = span!(Level::ERROR, "cache");
            spawn(async move { state.try_chat(p).instrument(cache_span).await });
        }
        None
    }

    async fn check_empty_choices(
        &self,
        resp: wreq::Response,
    ) -> Result<impl Stream<Item = Result<Bytes, wreq::Error>> + Send + 'static, ClewdrError> {
        if self.stream {
            return Ok(Either::Left(resp.bytes_stream()));
        }
        let bytes = resp.bytes().await.context(RquestSnafu {
            msg: "Failed to get bytes from Gemini response",
        })?;

        match self.api_format {
            GeminiApiFormat::Gemini => {
                let res = serde_json::from_slice::<GeminiResponse>(&bytes)?;
                if res.candidates.is_empty() {
                    return Err(ClewdrError::EmptyChoices);
                }
                if res.candidates[0].finishReason == Some(FinishReason::OTHER) {
                    return Err(ClewdrError::EmptyChoices);
                }
            }
            GeminiApiFormat::OpenAI => {
                let res = serde_json::from_slice::<Value>(&bytes)?;
                if res["choices"].as_array().is_some_and(|v| v.is_empty()) {
                    return Err(ClewdrError::EmptyChoices);
                }
                if res["choices"][0]["finish_reason"] == "OTHER" {
                    return Err(ClewdrError::EmptyChoices);
                }
            }
        }
        Ok(Either::Right(stream::once(async { Ok(bytes) })))
    }
}

async fn transform_response(
    cache_key: Option<(u64, usize)>,
    input: impl Stream<Item = Result<Bytes, wreq::Error>> + Send + 'static,
) -> axum::response::Response {
    // response is used for caching
    if let Some((key, id)) = cache_key {
        CACHE.push(input, key, id);
        // return whatever, not used
        return Body::empty().into_response();
    }
    // response is used for returning
    // not streaming
    // stream the response
    Body::from_stream(input).into_response()
}
