//! Test-support binary: the scripted fake runtime (see fake_runtime.rs).

#[tokio::main(flavor = "current_thread")]
async fn main() {
    dsh_harness_client::fake_runtime::serve().await;
}
