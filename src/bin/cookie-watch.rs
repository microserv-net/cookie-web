//! `cookie-watch` — the watchdog service: always answers whether cookie-web is up.
//! See `cookie_web::watch`.

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("RUST_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("cookie_web=info")),
        )
        .init();
    cookie_web::watch::serve().await
}
