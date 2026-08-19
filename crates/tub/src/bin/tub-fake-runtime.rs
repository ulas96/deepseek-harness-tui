//! Test-support binary for tub's snapshot tests: the scripted fake runtime
//! from dsh-harness-client, so tests can drive a real client + transport
//! stack against a deterministic peer.

#[tokio::main(flavor = "current_thread")]
async fn main() {
    dsh_harness_client::fake_runtime::serve().await;
}
