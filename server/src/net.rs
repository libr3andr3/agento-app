//! Shared HTTP client construction.
//!
//! reqwest has NO default request timeout: a hung upstream (LLM provider,
//! Whisper, the WA bridge) would otherwise pin the whole customer turn until
//! the APK's own 180-second read timeout gives up. Every outbound client is
//! built here so a total timeout is impossible to forget.

use std::time::Duration;

pub fn client(total: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(total)
        .build()
        .expect("reqwest client construction cannot fail with these options")
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn total_timeout_applies() {
        // A listener that accepts and never answers.
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { let _keep = l.accept().await; tokio::time::sleep(std::time::Duration::from_secs(30)).await; });
        let t = std::time::Instant::now();
        let r = super::client(std::time::Duration::from_millis(200)).get(format!("http://{addr}/")).send().await;
        assert!(r.unwrap_err().is_timeout());
        assert!(t.elapsed() < std::time::Duration::from_secs(5));
    }
}
