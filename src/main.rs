use anyhow::Context;
use clap::Parser;
use divvun_runtime::{bundle::Bundle, modules::PipelineValue, util::parse_accept_language};
use futures_util::StreamExt;
use poem::{
    get, handler,
    http::StatusCode,
    listener::TcpListener,
    middleware::Cors,
    post,
    web::{Data, Html, Json, Query},
    EndpointExt, IntoResponse, Request, Route, Server,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{path::Path, sync::Arc, time::Duration};

#[derive(serde::Deserialize)]
struct ProcessInput {
    text: String,
    ignore: Option<Vec<String>>,
    ignore_tags: Option<Vec<String>>,
}

#[derive(Deserialize, Serialize, Clone)]
pub struct GramcheckErrResponse {
    pub error_text: String,
    pub start_index: u32,
    pub end_index: u32,
    pub error_code: String,
    pub description: String,
    pub suggestions: Vec<String>,
    pub title: String,
}

#[derive(Deserialize, Serialize, Clone)]
pub struct GramcheckResponse {
    pub text: String,
    pub errs: Vec<GramcheckErrResponse>,
}

#[derive(Deserialize)]
struct ProcessQuery {
    encoding: Option<String>,
}

#[handler]
async fn preferences_get(
    Data(bundle): Data<&Arc<Bundle>>,
    Data(lang): Data<&Language>,
    req: &Request,
) -> impl IntoResponse {
    // Extract and parse Accept-Language header for locale configuration
    let mut locales = if let Some(accept_lang) = req.header("Accept-Language") {
        parse_accept_language(accept_lang)
            .into_iter()
            .map(|(lang_id, _)| lang_id.to_string())
            .collect::<Vec<String>>()
    } else {
        Vec::new()
    };

    // Add default language as fallback if not already present
    if let Language(Some(lang)) = lang {
        if !locales.contains(&lang) {
            locales.push(lang.to_string());
        }
    }

    let Some((_, suggest)) = bundle.command::<divvun_runtime::modules::divvun::Suggest>(None)
    else {
        tracing::error!("Suggest command not found in bundle");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };

    let locales = locales.iter().map(|x| &**x).collect::<Vec<&str>>();
    let prefs = suggest.error_preferences(&locales);

    Json(json!({
        "error_tags": prefs,
    }))
    .into_response()
}

async fn process(
    Data(bundle): Data<&Arc<Bundle>>,
    Data(lang): Data<&Language>,
    Json(body): Json<ProcessInput>,
    Query(query): Query<ProcessQuery>,
    req: &Request,
) -> impl IntoResponse {
    let text = body.text.trim();
    let is_utf16 = match query.encoding.as_deref() {
        Some("utf-16") | None => true,
        Some("utf-8") => false,
        Some(enc) => {
            tracing::error!("Unsupported encoding: {}", enc);
            return StatusCode::BAD_REQUEST.into_response();
        }
    };

    // Extract and parse Accept-Language header for locale configuration
    let mut locales = if let Some(accept_lang) = req.header("Accept-Language") {
        parse_accept_language(accept_lang)
            .into_iter()
            .map(|(lang_id, _)| lang_id.to_string())
            .collect::<Vec<String>>()
    } else {
        Vec::new()
    };

    // Add default language as fallback if not already present
    if let Language(Some(lang)) = lang {
        if !locales.contains(lang) {
            locales.push(lang.to_string());
        }
    }

    // Build configuration with locales for suggestions
    let mut suggest_config = serde_json::json!({
        "locales": locales,
        "encoding": if is_utf16 { "utf-16" } else { "utf-8" },
    });

    // Handle ignore list - prefer 'ignore' over deprecated 'ignore_tags'
    let ignore_list = body.ignore.as_ref().or(body.ignore_tags.as_ref());
    if let Some(ignore_list) = ignore_list {
        if !ignore_list.is_empty() {
            suggest_config["ignore"] = serde_json::json!(ignore_list);
        }
    }

    tracing::info!("process: text={:?}, encoding={}", text, if is_utf16 { "utf-16" } else { "utf-8" });

    let Some((id, _)) = bundle.command::<divvun_runtime::modules::divvun::Suggest>(None) else {
        tracing::error!("Suggest command not found in bundle");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };

    let config = serde_json::json!({
        id: suggest_config
    });

    tracing::debug!("process: config={config}");
    tracing::debug!("process: creating pipeline");
    let mut pipeline = match bundle.create(config).await {
        Ok(pipeline) => pipeline,
        Err(e) => {
            tracing::error!("Failed to create pipeline: {:?}", e);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    tracing::debug!("process: pipeline created, forwarding input");
    let mut stream = pipeline.forward(PipelineValue::String(text.to_string())).await;

    tracing::debug!("process: awaiting stream.next()");
    let output = match stream.next().await {
        Some(output) => match output {
            Ok(output) => {
                tracing::debug!("process: got pipeline output");
                output
            }
            Err(e) => {
                tracing::error!("Failed to process text: {:?}", e);
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        },
        None => {
            tracing::error!("No output from pipeline");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let (result_text, result_errs) = match output {
        PipelineValue::Json(s) => match s {
            serde_json::Value::Object(obj) => {
                let text = obj
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or(text)
                    .to_string();
                let errs = match obj.get("errors") {
                    Some(serde_json::Value::Array(x)) => x.clone(),
                    _ => {
                        tracing::error!("Expected 'errors' array in pipeline output");
                        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                    }
                };
                (text, errs)
            }
            _ => {
                tracing::error!("Expected JSON object from pipeline");
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        },
        x => {
            tracing::error!("{:?}", x);
            tracing::error!("Unexpected output type from pipeline");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    tracing::debug!("Pipeline output: {:?}", result_errs);

    let result = result_errs
        .iter()
        .filter_map(|obj| {
            let form = obj.get("form")?.as_str()?.to_string();
            let beg = obj.get("start")?.as_u64()? as u32;
            let end = obj.get("end")?.as_u64()? as u32;
            let err = obj.get("error_id")?.as_str()?.to_string();
            let title = obj
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let description = obj
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let suggestions = obj
                .get("suggestions")?
                .as_array()?
                .iter()
                .filter_map(|s| s.as_str().map(|s| s.to_string()))
                .collect();

            Some(GramcheckErrResponse {
                error_text: form,
                start_index: beg,
                end_index: end,
                error_code: err,
                title,
                description,
                suggestions,
            })
        })
        .collect::<Vec<_>>();

    Json(GramcheckResponse {
        text: result_text,
        errs: result,
    })
    .into_response()
}

const PAGE: &str = include_str!("../index.html");

#[derive(Debug, Clone)]
struct Language(Option<String>);

#[handler]
async fn process_get(Data(lang): Data<&Language>) -> impl IntoResponse {
    Html(PAGE.replace("%LANG%", &lang.0.as_deref().unwrap_or("unknown"))).into_response()
}

#[handler]
async fn process_post(
    bundle: Data<&Arc<Bundle>>,
    lang: Data<&Language>,
    body: Json<ProcessInput>,
    query: Query<ProcessQuery>,
    req: &Request,
) -> impl IntoResponse {
    process(bundle, lang, body, query, req).await
}

/// Liveness/readiness endpoint. Deliberately does NOT run the grammar pipeline.
///
/// It used to call `process()` on the literal string "health check". Because
/// Kubernetes probes both point here, that made probes roughly 30x the real
/// request volume — 360 grammar passes an hour per replica against 9-14 actual
/// user requests. Every one of them occupied the worker.
///
/// The failure mode that caused was not subtle: a real document arriving while
/// probes were queued pushed the probe past its timeout, so liveness SIGKILLed a
/// perfectly healthy process (exit 137, nothing in the logs) and readiness pulled
/// every replica out of the load balancer at the same instant, returning 503s.
/// The health check was the outage.
///
/// Checking the shared state is what a probe actually needs here: the bundle is
/// loaded once at startup and `main` returns an error if that fails, so the
/// server never binds without a valid bundle. If these are present, the process
/// is up, its state is intact, and it is accepting connections.
///
/// This intentionally cannot detect a wedged pipeline. Running a full pass 6
/// times a minute to catch that is a bad trade — it caused far more downtime
/// than it ever detected. A pipeline that can wedge should be fixed there.
#[handler]
async fn health_check(req: &Request) -> impl IntoResponse {
    if req.data::<Arc<Bundle>>().is_none() || req.data::<Language>().is_none() {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    StatusCode::OK.into_response()
}

#[derive(Parser, Clone)]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// Path to the grammar bundle file (.drb)
    #[arg(required = true)]
    bundle_path: String,

    /// Default language for localizations (overrides bundle filename)
    #[arg(long, env = "DEFAULT_LANGUAGE")]
    language: Option<String>,

    /// Host to bind the server to
    #[arg(long, env = "HOST", default_value = "127.0.0.1")]
    host: String,

    /// Port to run the server on
    #[arg(long, env = "PORT", default_value_t = 4000)]
    port: u16,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();

    loop {
        match run(cli.clone()).await {
            Ok(_) => {
                tracing::info!("Server stopped, restarting...");
            }
            Err(e) => {
                tracing::error!("Server error: {:?}", e);
                return Err(e);
            }
        }
    }
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    let path = Path::new(&cli.bundle_path)
        .canonicalize()
        .context("Failed to canonicalize bundle path")?;

    tracing::info!("Loading grammar bundle from: {}", path.display());

    let initial_mtime = std::fs::metadata(&path)
        .context("Failed to read bundle file metadata")?
        .modified()
        .context("Failed to get modification time")?;

    let bundle = Arc::new(
        Bundle::from_bundle(&path)
            .await
            .context("Failed to load grammar bundle - ensure the .drb file is valid")?,
    );

    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);

    let watcher_path = path.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(5));
        loop {
            interval.tick().await;

            match std::fs::metadata(&watcher_path) {
                Ok(metadata) => match metadata.modified() {
                    Ok(mtime) if mtime != initial_mtime => {
                        tracing::info!("Bundle file changed, triggering restart");
                        let _ = shutdown_tx.send(true);
                        break;
                    }
                    Err(e) => {
                        tracing::error!("Failed to get modification time: {}", e);
                    }
                    _ => {}
                },
                Err(e) => {
                    tracing::error!("Failed to read bundle file metadata: {}", e);
                }
            }
        }
    });

    let app = Route::new()
        .at("/", post(process_post).get(process_get))
        .at("/preferences", get(preferences_get))
        .at("/health", get(health_check))
        .data(bundle)
        .data(Language(cli.language))
        .with(Cors::default());

    Server::new(TcpListener::bind((cli.host, cli.port)))
        .run_with_graceful_shutdown(
            app,
            async move {
                let _ = shutdown_rx.wait_for(|&v| v).await;
            },
            Some(Duration::from_secs(30)),
        )
        .await?;

    Ok(())
}
