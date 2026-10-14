use anyhow::{Context, Result};
use std::{
    process::{Child, Command, Stdio},
    time::Duration,
};

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_frpc-rust"));
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    command
}
#[tokio::test]
async fn cli_verify_status_and_stop_flushes_response() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let config = dir.path().join("frpc.toml");
    let port = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await?
        .local_addr()?
        .port();
    // Keep the upstream port occupied but do not accept login. Shutdown must
    // remain responsive while the client is waiting for its initial handshake.
    let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    std::fs::write(
        &config,
        format!(
            "serverPort={}\nloginFailExit=false\nwebServer.port={port}\n",
            upstream.local_addr()?.port()
        ),
    )?;
    assert!(
        command()
            .args(["-c", config.to_str().unwrap(), "verify"])
            .status()?
            .success()
    );
    let mut client = Process(command().args(["-c", config.to_str().unwrap()]).spawn()?);
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .no_proxy()
        .build()?;
    let url = format!("http://127.0.0.1:{port}");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if http.get(format!("{url}/api/status")).send().await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?;
    assert!(
        command()
            .args(["-c", config.to_str().unwrap(), "status"])
            .status()?
            .success()
    );
    let response = http.post(format!("{url}/api/stop")).send().await?;
    assert_eq!(response.status(), 200);
    let json: serde_json::Value = response.json().await?;
    assert!(json["message"].as_str().is_some());
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(status) = client.0.try_wait()? {
                assert!(status.success());
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("client did not stop")??;
    Ok(())
}
