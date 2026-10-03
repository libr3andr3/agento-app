use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();
    if std::env::var("BIND_ADDR").is_err() {
        std::env::set_var("BIND_ADDR", "0.0.0.0:8118");
    }
    agente_core::run(|_| {}).await
}
