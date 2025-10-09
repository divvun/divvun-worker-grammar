use anyhow::Context;
use clap::Parser;
use divvun_runtime::{bundle::Bundle, modules::Input, util::parse_accept_language};
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
use std::{path::Path, sync::Arc};

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

    let Some(suggest) = bundle.command::<divvun_runtime::modules::divvun::Suggest>("suggest")
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

    let config = serde_json::json!({
        "suggest": suggest_config
    });

    let mut pipeline = match bundle.create(config).await {
        Ok(pipeline) => pipeline,
        Err(e) => {
            tracing::error!("Failed to create pipeline: {:?}", e);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let mut stream = pipeline.forward(Input::String(text.to_string())).await;

    let output = match stream.next().await {
        Some(output) => match output {
            Ok(output) => output,
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

    let result_json = match output {
        Input::Json(s) => match s {
            serde_json::Value::Array(x) => x,
            _ => {
                tracing::error!("Expected JSON array from pipeline");
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        },
        x => {
            tracing::error!("{:?}", x);
            tracing::error!("Unexpected output type from pipeline");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    tracing::debug!("Pipeline output: {:?}", result_json);

    let result = result_json
        .iter()
        .filter_map(|obj| {
            let form = obj.get("form")?.as_str()?.to_string();
            let beg = obj.get("beg")?.as_u64()? as u32;
            let end = obj.get("end")?.as_u64()? as u32;
            let err = obj.get("err")?.as_str()?.to_string();
            let msg = obj.get("msg")?.as_array()?;
            let rep = obj
                .get("rep")?
                .as_array()?
                .iter()
                .filter_map(|s| s.as_str().map(|s| s.to_string()))
                .collect();

            Some(GramcheckErrResponse {
                error_text: form,
                start_index: beg,
                end_index: end,
                error_code: err,
                title: msg.get(0)?.as_str()?.to_string(),
                description: msg
                    .get(1)
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                suggestions: rep,
            })
        })
        .collect::<Vec<_>>();

    Json(GramcheckResponse {
        text: text.to_string(),
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

#[handler]
async fn health_check(req: &Request) -> impl IntoResponse {
    let Some(bundle) = req.data::<Arc<Bundle>>() else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };

    let Some(lang) = req.data::<Language>() else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };

    let body = ProcessInput {
        text: "".to_string(),
        ignore: None,
        ignore_tags: None,
    };

    let query = ProcessQuery { encoding: None };

    let res = process(Data(bundle), Data(lang), Json(body), Query(query), req)
        .await
        .into_response();
    res.status().into_response()
}

#[derive(Parser)]
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
    let cli = Cli::parse();
    Ok(run(cli).await?)
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let path = Path::new(&cli.bundle_path)
        .canonicalize()
        .context("Failed to canonicalize bundle path")?;

    tracing::info!("Loading grammar bundle from: {}", path.display());

    let bundle = Arc::new(
        Bundle::from_bundle(&path)
            .context("Failed to load grammar bundle - ensure the .drb file is valid")?,
    );

    let app = Route::new()
        .at("/", post(process_post).get(process_get))
        .at("/preferences", get(preferences_get))
        .at("/health", get(health_check))
        .data(bundle)
        .data(Language(cli.language))
        .with(Cors::default());

    Server::new(TcpListener::bind((cli.host, cli.port)))
        .run(app)
        .await?;

    Ok(())
}
