//! Guards shared by the local-development binaries (`local-tables`, `local-seed`), and `poem-local`'s `/dev` routes.
//!
//! These binaries create tables and write rows unconditionally, which is only ever
//! safe against a DynamoDB running on this machine. The check lives here rather
//! than in each binary so the two cannot drift apart.

use anyhow::{Result, anyhow, bail};
use poem::endpoint::BoxEndpoint;
use poem::http::StatusCode;
use poem::web::{Data, Json, Query};
use poem::{EndpointExt, Request, Route, get, handler, post};
use std::sync::atomic::{AtomicU64, Ordering};

/// The configured DynamoDB endpoint, but only if it is unmistakably a local one.
///
/// This is the whole safety story for the local-dev binaries: with no endpoint
/// override the AWS SDK talks to real DynamoDB using whatever credentials are
/// lying around, and a stray `DB_PREFIX` would then create or overwrite rows in a
/// real account.
pub fn require_local_dynamodb_endpoint() -> Result<String> {
    let endpoint = std::env::var("AWS_ENDPOINT_URL_DYNAMODB")
        .or_else(|_| std::env::var("AWS_ENDPOINT_URL"))
        .map_err(|_| {
            anyhow!(
                "AWS_ENDPOINT_URL_DYNAMODB is not set — refusing to run in case this would \
                 write to a real AWS account. Use the `make local-*` targets, which point it \
                 at the DynamoDB Local started by local/dynamodb.sh."
            )
        })?;
    local_endpoint(endpoint)
}

/// `endpoint`, if its host is this machine. Split from the env read so it tests without mutating env.
fn local_endpoint(endpoint: String) -> Result<String> {
    let url = url::Url::parse(&endpoint)
        .map_err(|e| anyhow!("AWS_ENDPOINT_URL_DYNAMODB is not a URL: {e}"))?;
    match url.host_str() {
        Some(host) if is_local_url_host(host) => Ok(endpoint),
        other => bail!(
            "DynamoDB endpoint {:?} is not local (host {:?}) — refusing to run.",
            endpoint,
            other.unwrap_or("<none>")
        ),
    }
}

/// Extra GraphQL response delay set from the dev bar. Only `/dev/lag` writes it, so it stays 0 without `/dev`.
static EXTRA_LAG_MS: AtomicU64 = AtomicU64::new(0);

pub fn extra_lag_ms() -> u64 {
    EXTRA_LAG_MS.load(Ordering::Relaxed)
}

/// A URL host naming this machine.
fn is_local_url_host(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]")
}

/// A DynamoDB client for the given endpoint-and-region environment.
pub async fn dynamodb_client() -> aws_sdk_dynamodb::Client {
    let config = crate::aws_config_loader()
        .region(aws_config::Region::new(
            std::env::var("AWS_REGION").unwrap_or_else(|_| "ap-southeast-2".to_string()),
        ))
        .load()
        .await;
    aws_sdk_dynamodb::Client::new(&config)
}

/// `/dev/*` shortcuts for the web dev bar (web/src/dev/DevBar.tsx), mounted by `poem-local` only.
///
/// Off unless the server is on `seslogin_local` at a local endpoint; every request must come from this machine.
/// Writes go through the `local-seed` and `cli` binaries, so each shortcut is exactly the command it replaces.
pub fn dev_routes(db_prefix: &str, mail: crate::mockmail::Handler) -> Option<BoxEndpoint<'static>> {
    if !dev_routes_allowed(db_prefix, require_local_dynamodb_endpoint()) {
        tracing::info!("dev routes off: they need DB_PREFIX=seslogin_local on a local DynamoDB");
        return None;
    }
    tracing::warn!("dev routes on at /dev, localhost callers only");
    Some(
        Route::new()
            .at("/seed", post(seed))
            .at("/clear", post(clear))
            .at("/tokens", post(tokens))
            .at("/summary", post(summary))
            .at("/signin", post(signin))
            .at("/mail", get(last_mail))
            .at("/reactivate", post(reactivate))
            .at("/period-link", post(period_link))
            .at("/lag", post(set_lag))
            .at("/errors", post(set_errors))
            .at("/status", get(status))
            .before(local_only)
            .data(mail)
            .boxed(),
    )
}

/// Refuse callers not on this machine (the API listens on every interface), and pages from
/// other sites in this machine's browser, which could otherwise post here cross-site.
async fn local_only(req: Request) -> poem::Result<Request> {
    if caller_allowed(req.remote_addr().as_socket_addr(), req.header("origin")) {
        Ok(req)
    } else {
        Err(poem::Error::from_status(StatusCode::FORBIDDEN))
    }
}

/// Mount `/dev` only for the local tables on a local DynamoDB endpoint.
fn dev_routes_allowed(db_prefix: &str, endpoint: Result<String>) -> bool {
    db_prefix == "seslogin_local" && endpoint.is_ok()
}

/// A loopback caller, with no `Origin` (curl) or a local one (the dev server's pages).
fn caller_allowed(remote: Option<&std::net::SocketAddr>, origin: Option<&str>) -> bool {
    remote.is_some_and(|a| a.ip().is_loopback())
        && origin.is_none_or(|o| {
            url::Url::parse(o)
                .ok()
                .and_then(|u| u.host_str().map(is_local_url_host))
                .unwrap_or(false)
        })
}

/// Run a sibling binary via `cargo run`; returns its combined output.
async fn run_bin(bin: &str, args: &[String]) -> poem::Result<String> {
    let out = tokio::process::Command::new("cargo")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .args(["run", "--quiet", "--bin", bin, "--"])
        // Plain logs: the output lands in the dev bar, not a terminal.
        .env("NO_COLOR", "1")
        .args(args)
        .output()
        .await
        .map_err(poem::error::InternalServerError)?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    if out.status.success() {
        Ok(text)
    } else {
        Err(poem::Error::from_string(
            text,
            StatusCode::INTERNAL_SERVER_ERROR,
        ))
    }
}

/// `make local-seed`
#[handler]
async fn seed() -> poem::Result<String> {
    run_bin("local-seed", &["apply".into()]).await
}

/// `make local-clear`
#[handler]
async fn clear() -> poem::Result<String> {
    run_bin("local-seed", &["clear".into()]).await
}

/// `local-seed apply --table user_token`: the seeded logins again, eg after Log out deleted one.
#[handler]
async fn tokens() -> poem::Result<String> {
    run_bin("local-seed", &["apply".into(), "--table=user_token".into()]).await
}

/// `activity-summary` dry run: yesterday's daily summary emails, printed rather than sent.
#[handler]
async fn summary() -> poem::Result<String> {
    let out = run_bin("activity-summary", &[]).await?;
    if out.contains("--- DRY RUN") {
        return Ok(out);
    }
    Ok(format!(
        "{out}\nNo summary to send: it needs a user subscribed to daily summaries (admin Settings) \
         and activity yesterday at their unit."
    ))
}

#[derive(serde::Deserialize)]
struct SigninParams {
    person: String,
    location: String,
    session: Option<String>,
    hours_ago: Option<f64>,
}

/// `cli period create-signin ... --dry-run false`; `--flag=value` so a value can't become a flag.
#[handler]
async fn signin(Query(p): Query<SigninParams>) -> poem::Result<String> {
    let mut args = vec![
        "period".into(),
        "create-signin".into(),
        format!("--person={}", p.person),
        format!("--location={}", p.location),
        "--dry-run=false".into(),
    ];
    args.extend(p.session.map(|s| format!("--session={s}")));
    args.extend(p.hours_ago.map(|h| format!("--hours-ago={h}")));
    run_bin("cli", &args).await
}

#[derive(serde::Deserialize)]
struct SessionParams {
    session: String,
}

/// `cli session edit --require-reactivation --dry-run false`
#[handler]
async fn reactivate(Query(p): Query<SessionParams>) -> poem::Result<String> {
    let args = [
        "session",
        "edit",
        "--require-reactivation",
        "--dry-run=false",
        "--",
    ];
    let mut args: Vec<String> = args.map(String::from).to_vec();
    args.push(p.session);
    run_bin("cli", &args).await
}

#[derive(serde::Deserialize)]
struct PeriodParams {
    period: String,
}

/// `cli period-link issue`: a member's edit link for the period, against this site.
#[handler]
async fn period_link(Query(p): Query<PeriodParams>) -> poem::Result<String> {
    let base = format!("--base-url={}", crate::base_url::web_base_url());
    run_bin(
        "cli",
        &[
            "period-link".into(),
            "issue".into(),
            base,
            "--".into(),
            p.period,
        ],
    )
    .await
}

#[derive(serde::Deserialize)]
struct LagParams {
    ms: u64,
}

/// `--response-lag-ms`, without the restart.
#[handler]
async fn set_lag(Query(p): Query<LagParams>) -> String {
    EXTRA_LAG_MS.store(p.ms, Ordering::Relaxed);
    format!("Every GraphQL response now waits an extra {}ms", p.ms)
}

#[derive(serde::Deserialize)]
struct ErrorParams {
    #[serde(default)]
    targets: String,
}

/// `SESLOGIN_FORCE_FIELD_ERRORS`, without the restart; empty clears. Dev builds only, like the injector.
#[handler]
async fn set_errors(Query(p): Query<ErrorParams>) -> poem::Result<String> {
    #[cfg(debug_assertions)]
    return Ok(
        match crate::graphql::error_injection::set_runtime_targets(&p.targets) {
            0 if p.targets.trim().is_empty() => "Error injection off".to_string(),
            0 => {
                return Err(poem::Error::from_string(
                    format!("No valid targets in `{}`", p.targets),
                    StatusCode::BAD_REQUEST,
                ));
            }
            n => format!(
                "Failing {n} target(s): {}",
                crate::graphql::error_injection::runtime_targets()
            ),
        },
    );
    #[cfg(not(debug_assertions))]
    Err(poem::Error::from_string(
        format!(
            "error injection is compiled out of release builds (asked for `{}`)",
            p.targets
        ),
        StatusCode::NOT_IMPLEMENTED,
    ))
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct DevStatus {
    lag_ms: u64,
    error_targets: String,
}

/// The runtime toggles' current values, for the dev bar.
#[handler]
async fn status() -> Json<DevStatus> {
    #[cfg(debug_assertions)]
    let error_targets = crate::graphql::error_injection::runtime_targets();
    #[cfg(not(debug_assertions))]
    let error_targets = String::new();
    Json(DevStatus {
        lag_ms: extra_lag_ms(),
        error_targets,
    })
}

/// Newest message the mock mailer has "sent", eg a login code.
#[handler]
async fn last_mail(mail: Data<&crate::mockmail::Handler>) -> poem::Result<String> {
    Ok(mail.sent().last().map_or_else(
        || "No mail sent since the API started".to_string(),
        |m| format!("To: {}\nSubject: {}\n\n{}", m.to, m.subject, m.body),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOCAL: &str = "http://localhost:8100";

    #[test]
    fn dev_routes_only_for_local_tables_on_local_dynamodb() {
        let local = || local_endpoint(LOCAL.into());
        assert!(dev_routes_allowed("seslogin_local", local()));
        for prefix in ["seslogin_prod", "seslogin_test", "", "seslogin_local_old"] {
            assert!(
                !dev_routes_allowed(prefix, local()),
                "{prefix} must not mount /dev"
            );
        }
        for endpoint in [
            "https://dynamodb.ap-southeast-2.amazonaws.com",
            "http://192.0.2.10:8100",
            "not a url",
        ] {
            assert!(
                !dev_routes_allowed("seslogin_local", local_endpoint(endpoint.into())),
                "{endpoint} must not mount /dev"
            );
        }
        assert!(!dev_routes_allowed("seslogin_local", Err(anyhow!("unset"))));
    }

    #[test]
    fn dev_callers_must_be_this_machine_and_a_local_page() {
        let v4 = "127.0.0.1:50000".parse().unwrap();
        let v6 = "[::1]:50000".parse().unwrap();
        let lan = "192.0.2.10:50000".parse().unwrap();
        assert!(caller_allowed(Some(&v4), None));
        assert!(caller_allowed(Some(&v6), Some("http://localhost:5173")));
        assert!(caller_allowed(Some(&v4), Some("http://127.0.0.1:5173")));
        assert!(!caller_allowed(Some(&lan), None), "another machine");
        assert!(!caller_allowed(None, None), "unknown address");
        for origin in [
            "https://example.com",
            "http://localhost.example.com",
            "null",
        ] {
            assert!(
                !caller_allowed(Some(&v4), Some(origin)),
                "{origin} is another site"
            );
        }
    }

    /// Only `poem-local` may mount `/dev`: the real server and the Lambda must never reference it.
    #[test]
    fn deployed_servers_never_mount_dev_routes() {
        let src = |rel: &str| {
            std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(rel))
                .unwrap_or_else(|e| panic!("reading {rel}: {e}"))
        };
        let poem = src("src/bin/poem.rs");
        assert!(!poem.contains("dev_routes") && !poem.contains("local_dev"));
        assert!(
            poem.contains("realtime, None).await"),
            "poem.rs must pass no dev routes"
        );
        let lambda_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/bin/lambda");
        for entry in std::fs::read_dir(&lambda_dir).expect("reading src/bin/lambda") {
            let path = entry.expect("dir entry").path();
            let text = std::fs::read_to_string(&path).expect("reading a lambda source");
            assert!(
                !text.contains("local_dev"),
                "{} references local_dev",
                path.display()
            );
        }
    }
}
