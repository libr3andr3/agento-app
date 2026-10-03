//! Pairing a device to this core (DECISIONS D3). Loopback only, app-key
//! gated like every /api route: the owner's own shell asks for a code and
//! shows it; the other device proves it over the relay with `pair`.

use super::*;

/// `POST /api/pair/start` → `{code, expiresAt, expiresInSecs}`; replaces any open code.
pub(super) async fn pair_start(State(state): State<SharedState>) -> ApiResult {
    Ok(Json(crate::owner::start_pairing(&state)))
}

/// `GET /api/pair/code` → the open code, or 404 when none is open.
pub(super) async fn pair_code(State(state): State<SharedState>) -> ApiResult {
    crate::owner::current_pairing(&state)
        .map(Json)
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "no pairing code is open — POST /api/pair/start"))
}

#[cfg(test)]
mod tests {
    use crate::testkit::{self, APP_KEY};

    #[tokio::test]
    async fn codes_are_minted_and_read_on_the_loopback() {
        let s = testkit::state().await;
        assert_eq!(testkit::api(&s, "GET", "/api/pair/code", None, None).await.0, 404);
        let (st, v) = testkit::api(&s, "POST", "/api/pair/start", None, None).await;
        assert_eq!(st, 200);
        let (st, w) = testkit::api(&s, "GET", "/api/pair/code", None, None).await;
        assert_eq!((st, w["code"].clone()), (200, v["code"].clone()));
        assert_eq!(st, 200);
    }

    /// The app key is not a secret on a public node (every APK carries the
    /// relay's). A pairing code is owner access: never over the network.
    #[tokio::test]
    async fn remote_callers_cannot_mint_or_read_a_code() {
        let s = testkit::state().await;
        let remote: std::net::SocketAddr = ([203, 0, 113, 7], 5000).into();
        let h = [("x-app-key", APP_KEY)];
        assert_eq!(testkit::call_from(&s, remote, "POST", "/api/pair/start", &h, None).await.0, 403);
        assert!(crate::owner::current_pairing(&s).is_none());
        crate::owner::start_pairing(&s);
        let (st, v) = testkit::call_from(&s, remote, "GET", "/api/pair/code", &h, None).await;
        assert_eq!(st, 403, "{v}");
        assert!(v.get("code").is_none());
        // A proxy header does not make a remote caller local.
        let h = [("x-app-key", APP_KEY), ("x-forwarded-for", "127.0.0.1")];
        assert_eq!(testkit::call_from(&s, remote, "GET", "/api/pair/code", &h, None).await.0, 403);
    }
}
