//! Standalone fake Vault (OIDC token minting) for the local demo.
//!
//! Wraps `upstream_fixture::FakeVault::start_minting` as a binary so a
//! shell harness can spawn it. It mints the literal token given as the
//! first argument (default `dev-token` — which the demo agent accepts in
//! its dev-token mode), prints its URL on the first stdout line (point
//! Triton's `TRITON_VAULT_URL` at it), then parks.
//!
//! Usage:  fake-vault [<minted-token>]

use triton_tests::upstream_fixture::FakeVault;

#[tokio::main]
async fn main() {
    let token = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "dev-token".into());
    // `start_minting` wants a 'static str; leak the small token string.
    let token: &'static str = Box::leak(token.into_boxed_str());
    let vault = FakeVault::start_minting(token).await;
    println!("{}", vault.url());

    std::future::pending::<()>().await;
}
