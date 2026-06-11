mod envfile;
mod store;

use axum::{
    Form, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Path, State},
    http::{StatusCode, header},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use clap::Parser;
use directories::BaseDirs;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, net::SocketAddr, path::PathBuf};
use store::{DEFAULT_KEY, Store, StoreError};
use tokio::net::TcpListener;

const DEFAULT_BIND: &str = "127.0.0.1:37997";
const INDEX_TEMPLATE: &str = include_str!("../templates/index.html");
const HELP_TEMPLATE: &str = include_str!("../templates/help.html");
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Parser)]
#[command(
    author,
    version,
    about = "A local-only development secret server for git worktrees"
)]
struct Args {
    /// Address to listen on. Keep this on loopback unless you understand the risk.
    #[arg(long, env = "LOCAL_SECRETS_BIND", default_value = DEFAULT_BIND)]
    bind: SocketAddr,

    /// Storage directory. Defaults to the OS-specific config directory for local-secrets.
    #[arg(long, env = "LOCAL_SECRETS_DIR")]
    data_dir: Option<PathBuf>,
}

#[derive(Debug, Clone)]
struct AppState {
    store: Store,
    public_base_url: String,
}

#[derive(Debug, Serialize)]
struct EnvResponse {
    variables: BTreeMap<String, String>,
}

#[derive(Debug, Serialize)]
struct FileListResponse {
    files: Vec<FileResponse>,
}

#[derive(Debug, Serialize)]
struct FileResponse {
    name: String,
    bytes: u64,
    url: String,
}

#[derive(Debug, Serialize)]
struct KeyListResponse {
    keys: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct NewKeyForm {
    name: String,
}

#[derive(Debug, Deserialize)]
struct RenameKeyForm {
    new_name: String,
}

#[derive(Debug, Deserialize)]
struct FileForm {
    name: String,
    contents: String,
}

#[derive(Debug, Deserialize)]
struct FileNameForm {
    name: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let store = Store::open(resolve_data_dir(args.data_dir)?)?;
    let state = AppState {
        store,
        public_base_url: format!("http://{}", args.bind),
    };

    let app = app(state.clone());
    let listener = TcpListener::bind(args.bind).await?;

    println!("local-secrets listening on {}", state.public_base_url);
    println!(
        "configuration UI: {}/keys/{}",
        state.public_base_url, DEFAULT_KEY
    );
    println!("storage directory: {}", state.store.paths().root.display());

    axum::serve(listener, app).await?;
    Ok(())
}

fn app(state: AppState) -> Router {
    Router::new()
        .route("/", get(root_redirect))
        .route("/favicon.ico", get(favicon))
        // HTML UI
        .route("/keys/{name}", get(index))
        .route("/help", get(help_modal))
        .route("/ui/keys", post(create_key_form))
        .route("/ui/keys/{name}/rename", post(rename_key_form))
        .route("/ui/keys/{name}/delete", post(delete_key_form))
        .route("/ui/keys/{name}/env", post(save_env_from_form))
        .route("/ui/keys/{name}/files", post(save_file_from_form))
        .route("/ui/keys/{name}/files/delete", post(delete_file_from_form))
        // API: keys
        .route("/api/keys", get(list_keys).post(create_key))
        .route(
            "/api/keys/{name}",
            axum::routing::delete(delete_key).patch(rename_key),
        )
        // API: per-key env
        .route("/api/keys/{name}/env", get(get_env_json))
        .route("/api/keys/{name}/env-file", get(get_env_file).put(put_env))
        .route("/api/keys/{name}/export", get(get_shell_exports))
        .route("/api/keys/{name}/export/fish", get(get_fish_exports))
        // API: per-key shared files
        .route("/api/keys/{name}/files", get(list_files))
        .route(
            "/api/keys/{name}/files/{filename}",
            get(get_file).put(put_file).delete(delete_file_api),
        )
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(state)
}

fn resolve_data_dir(override_dir: Option<PathBuf>) -> Result<PathBuf, std::io::Error> {
    if let Some(dir) = override_dir {
        return Ok(dir);
    }

    let dirs = BaseDirs::new().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "could not determine an OS config directory; pass --data-dir",
        )
    })?;

    Ok(dirs.config_dir().join("local-secrets"))
}

// ----- pages ---------------------------------------------------------------

async fn root_redirect() -> Response {
    (StatusCode::SEE_OTHER, [(header::LOCATION, "/keys/default")]).into_response()
}

async fn index(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Html<String>, AppError> {
    let env_file = state.store.read_env_file(&name).await?;
    let files = state.store.list_files(&name).await?;
    let keys = state.store.list_keys().await?;

    Ok(Html(render_index(&state, &name, &env_file, &files, &keys)))
}

async fn help_modal(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<BTreeMap<String, String>>,
) -> Result<Html<String>, AppError> {
    let key = params
        .get("key")
        .cloned()
        .unwrap_or_else(|| DEFAULT_KEY.to_string());
    store::validate_key_name(&key).map_err(AppError::from)?;
    let base = format!("{}/keys/{}", state.public_base_url, key);
    Ok(Html(HELP_TEMPLATE.replace("{{base}}", &html_escape(&base))))
}

async fn favicon() -> StatusCode {
    StatusCode::NO_CONTENT
}

// ----- API: keys -----------------------------------------------------------

async fn list_keys(State(state): State<AppState>) -> Result<axum::Json<KeyListResponse>, AppError> {
    let keys = state.store.list_keys().await?;
    Ok(axum::Json(KeyListResponse { keys }))
}

async fn create_key(
    State(state): State<AppState>,
    axum::Json(form): axum::Json<NewKeyForm>,
) -> Result<StatusCode, AppError> {
    state.store.create_key(&form.name).await?;
    Ok(StatusCode::CREATED)
}

async fn delete_key(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<StatusCode, AppError> {
    state.store.delete_key(&name).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn rename_key(
    State(state): State<AppState>,
    Path(name): Path<String>,
    axum::Json(form): axum::Json<RenameKeyForm>,
) -> Result<StatusCode, AppError> {
    state.store.rename_key(&name, &form.new_name).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn create_key_form(
    State(state): State<AppState>,
    Form(form): Form<NewKeyForm>,
) -> Result<Response, AppError> {
    state.store.create_key(&form.name).await?;
    Ok(redirect_to_key(&form.name))
}

async fn rename_key_form(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Form(form): Form<RenameKeyForm>,
) -> Result<Response, AppError> {
    state.store.rename_key(&name, &form.new_name).await?;
    Ok(redirect_to_key(&form.new_name))
}

async fn delete_key_form(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Response, AppError> {
    let keys = state.store.list_keys().await?;
    let next = keys
        .into_iter()
        .find(|k| k != &name)
        .unwrap_or_else(|| DEFAULT_KEY.to_string());
    state.store.delete_key(&name).await?;
    Ok(redirect_to_key(&next))
}

// ----- API: env ------------------------------------------------------------

async fn get_env_json(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<axum::Json<EnvResponse>, AppError> {
    let variables = state.store.read_env_map(&name).await?;
    Ok(axum::Json(EnvResponse { variables }))
}

async fn get_env_file(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Response, AppError> {
    let env_file = state.store.read_env_file(&name).await?;
    let served = envfile::to_served_env_file(&env_file)
        .map_err(|errors| StoreError::Validation(errors.join("\n")))?;
    Ok((
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        served,
    )
        .into_response())
}

async fn put_env(
    State(state): State<AppState>,
    Path(name): Path<String>,
    body: String,
) -> Result<StatusCode, AppError> {
    state.store.write_env_file(&name, &body).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn save_env_from_form(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Form(form): Form<BTreeMap<String, String>>,
) -> Result<Response, AppError> {
    let contents = form.get("env_file").cloned().unwrap_or_default();
    state.store.write_env_file(&name, &contents).await?;
    Ok(redirect_to_key(&name))
}

async fn get_shell_exports(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Response, AppError> {
    let variables = state.store.read_env_map(&name).await?;
    let exports = envfile::to_shell_exports(&variables);
    Ok((
        [(header::CONTENT_TYPE, "text/x-shellscript; charset=utf-8")],
        exports,
    )
        .into_response())
}

async fn get_fish_exports(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Response, AppError> {
    let variables = state.store.read_env_map(&name).await?;
    let exports = envfile::to_fish_exports(&variables);
    Ok((
        [(header::CONTENT_TYPE, "text/x-shellscript; charset=utf-8")],
        exports,
    )
        .into_response())
}

// ----- API: shared files ---------------------------------------------------

async fn list_files(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<axum::Json<FileListResponse>, AppError> {
    let files = state
        .store
        .list_files(&name)
        .await?
        .into_iter()
        .map(|file| FileResponse {
            url: format!("/api/keys/{}/files/{}", name, file.name),
            name: file.name,
            bytes: file.bytes,
        })
        .collect();
    Ok(axum::Json(FileListResponse { files }))
}

async fn get_file(
    State(state): State<AppState>,
    Path((name, filename)): Path<(String, String)>,
) -> Result<Response, AppError> {
    let bytes = state.store.read_named_file(&name, &filename).await?;
    Ok(([(header::CONTENT_TYPE, "application/octet-stream")], bytes).into_response())
}

async fn put_file(
    State(state): State<AppState>,
    Path((name, filename)): Path<(String, String)>,
    body: Bytes,
) -> Result<StatusCode, AppError> {
    state
        .store
        .write_named_file(&name, &filename, &body)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_file_api(
    State(state): State<AppState>,
    Path((name, filename)): Path<(String, String)>,
) -> Result<StatusCode, AppError> {
    state.store.delete_named_file(&name, &filename).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn save_file_from_form(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Form(form): Form<FileForm>,
) -> Result<Response, AppError> {
    state
        .store
        .write_named_file(&name, &form.name, form.contents.as_bytes())
        .await?;
    Ok(redirect_to_key(&name))
}

async fn delete_file_from_form(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Form(form): Form<FileNameForm>,
) -> Result<Response, AppError> {
    state.store.delete_named_file(&name, &form.name).await?;
    Ok(redirect_to_key(&name))
}

// ----- helpers -------------------------------------------------------------

fn redirect_to_key(name: &str) -> Response {
    let location = format!("/keys/{}", name);
    (StatusCode::SEE_OTHER, [(header::LOCATION, location)]).into_response()
}

#[derive(Debug)]
struct AppError(StoreError);

impl From<StoreError> for AppError {
    fn from(error: StoreError) -> Self {
        Self(error)
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = match self.0 {
            StoreError::BadRequest(_) | StoreError::Validation(_) => StatusCode::BAD_REQUEST,
            StoreError::NotFound(_) => StatusCode::NOT_FOUND,
            StoreError::Conflict(_) => StatusCode::CONFLICT,
            StoreError::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };

        let body = format!(
            "{}\n\nBack to local-secrets: /keys/{}",
            html_escape(&self.0.to_string()),
            DEFAULT_KEY
        );

        (
            status,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            body,
        )
            .into_response()
    }
}

fn render_index(
    state: &AppState,
    active_key: &str,
    env_file: &str,
    files: &[store::StoredFile],
    keys: &[String],
) -> String {
    let files_html = if files.is_empty() {
        "<p class=muted>No shared files saved yet.</p>".to_string()
    } else {
        let items = files
            .iter()
            .map(|file| {
                format!(
                    r#"<li><a href="/api/keys/{key}/files/{name}"><code>{name}</code></a> <span class="muted">({bytes} bytes)</span> <form method="post" action="/ui/keys/{key}/files/delete" class="inline"><input type="hidden" name="name" value="{name}"><button type="submit">Delete</button></form></li>"#,
                    key = html_escape(active_key),
                    name = html_escape(&file.name),
                    bytes = file.bytes,
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        format!("<ul>{items}</ul>")
    };

    let key_options = keys
        .iter()
        .map(|k| {
            let selected = if k == active_key { " selected" } else { "" };
            format!(
                r#"<option value="{k}"{selected}>{k}</option>"#,
                k = html_escape(k),
                selected = selected
            )
        })
        .collect::<Vec<_>>()
        .join("");

    let base = html_escape(&state.public_base_url);
    let env_path = html_escape(
        &state
            .store
            .paths()
            .env_file(active_key)
            .display()
            .to_string(),
    );
    let root_path = html_escape(&state.store.paths().root.display().to_string());
    let active = html_escape(active_key);
    let env_file = html_escape(env_file);

    INDEX_TEMPLATE
        .replace("{{base}}", &base)
        .replace("{{env_path}}", &env_path)
        .replace("{{root_path}}", &root_path)
        .replace("{{key_options}}", &key_options)
        .replace("{{active_key}}", &active)
        .replace("{{files_html}}", &files_html)
        .replace("{{env_file}}", &env_file)
}

fn html_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());

    for ch in value.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            _ => escaped.push(ch),
        }
    }

    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_html_metacharacters() {
        assert_eq!(html_escape("<&>\"'"), "&lt;&amp;&gt;&quot;&#39;");
    }
}
