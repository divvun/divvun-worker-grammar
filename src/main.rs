use clap::Parser;
use poem::{
    handler,
    http::StatusCode,
    listener::TcpListener,
    middleware::Cors,
    post,
    web::{Data, Html, Json},
    EndpointExt, IntoResponse, Route, Server,
};
use serde::{Deserialize, Serialize};
use std::{path::Path, sync::Arc};
use subterm::{SubprocessHandler as _, SubprocessPool};

#[derive(serde::Deserialize)]
struct ProcessInput {
    text: String,
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

#[handler]
async fn process(
    Data(pool): Data<&Arc<SubprocessPool>>,
    Json(body): Json<ProcessInput>,
) -> impl IntoResponse {
    let text = body.text.trim();
    let mut bundle = match pool.acquire().await {
        Ok(bundle) => bundle,
        Err(e) => {
            tracing::error!("{:?}", e);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    bundle.write_line(text).await.unwrap();
    bundle.flush().await.unwrap();
    let line = bundle.read_line().await.unwrap();

    let json: serde_json::Value = serde_json::from_str(&line).unwrap();
    let result = json
        .get("errs")
        .and_then(|errs| errs.as_array())
        .map(|x| {
            x.iter()
                .map(|x| GramcheckErrResponse {
                    error_text: x[0].as_str().unwrap().to_string(),
                    start_index: x[1].as_i64().unwrap() as u32,
                    end_index: x[2].as_i64().unwrap() as u32,
                    error_code: x[3].as_str().unwrap().to_string(),
                    description: x[4].as_str().unwrap().to_string(),
                    suggestions: x[5]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|s| s.as_str().unwrap().to_string())
                        .collect(),
                    title: x[6].as_str().unwrap().to_string(),
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    Json(GramcheckResponse {
        text: text.to_string(),
        errs: result,
    })
    .into_response()
}

const PAGE: &str = r#"
<!doctype html>
<html>
<head>
<title>Divvun Grammar</title>
<meta charset="utf-8">
<style>
.container {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 16px;
}

</style>
</head>
<body>
<div class="container">
<h2>Language: %LANG%</h2>
<textarea class="text"></textarea>
<div>
Result:
<pre class="result"></pre>
</div>
<button class="doit">
Run grammar
</button>
<script>
document.querySelector(".doit").addEventListener("click", () => {
    const text = document.querySelector(".text").value;
    fetch(location.href, {
        method: "POST",
        headers: {
            "Content-Type": "application/json",
        },
        body: JSON.stringify({ text }),
    }).then((r) => r.json()).then((r) => {
        document.querySelector(".result").textContent = JSON.stringify(r, null, 2);
    });
});
</script>
</div>
</body>
</html>
"#;

#[derive(Debug, Clone)]
struct Language(String);

#[handler]
async fn process_get(Data(lang): Data<&Language>) -> impl IntoResponse {
    Html(PAGE.replace("%LANG%", &lang.0)).into_response()
}

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// Path to the grammar bundle file
    #[arg(required = true)]
    bundle_path: String,

    /// Host to bind the server to
    #[arg(long, default_value = "127.0.0.1")]
    host: String,

    /// Port to run the server on
    #[arg(long, default_value_t = 4000)]
    port: u16,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    Ok(run(cli).await?)
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let path = Path::new(&cli.bundle_path).canonicalize().unwrap();
    let parent_path = path.parent().unwrap().to_path_buf();
    let file_name = path.file_name().unwrap().to_str().unwrap().to_string();
    let lang = file_name.split('.').next().unwrap().to_string();

    tracing::info!("Parent path: {}", parent_path.display());
    tracing::info!("File name: {}", file_name);

    let pool = subterm::SubprocessPool::new(
        move || {
            let mut cmd = tokio::process::Command::new("docker");
            cmd.args(["run", "-i", "-v"])
                .arg(format!("{}:/data", parent_path.display()))
                .args(["divvun-checker:latest", "divvun-checker", "-a"])
                .arg(format!("/data/{}", &file_name));
            cmd
        },
        4,
    )
    .await
    .unwrap();

    let app = Route::new()
        .at("/", post(process).get(process_get))
        .data(pool)
        .data(Language(lang))
        .with(Cors::default());

    Server::new(TcpListener::bind((cli.host, cli.port)))
        .run(app)
        .await?;

    Ok(())
}
