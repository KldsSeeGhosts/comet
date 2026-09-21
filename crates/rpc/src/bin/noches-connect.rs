//! Small gateway for existing engines. It can be updated independently of GPUI.
use std::{collections::HashMap, path::PathBuf, time::Duration};
use zeron_rpc::{
    methods,
    remote::{self, ConnectionProfile, Connections, Credentials, ServerOptions},
};

fn options() -> anyhow::Result<(String, HashMap<String, String>)> {
    let mut args = std::env::args().skip(1);
    let command = args.next().unwrap_or_else(|| "help".into());
    let mut opts = HashMap::new();
    while let Some(key) = args.next() {
        anyhow::ensure!(key.starts_with("--"), "Expected --option value");
        opts.insert(
            key,
            args.next()
                .ok_or_else(|| anyhow::anyhow!("Missing option value"))?,
        );
    }
    Ok((command, opts))
}
fn required(opts: &HashMap<String, String>, key: &str) -> anyhow::Result<String> {
    opts.get(key)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("Missing {key}"))
}
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let (command, opts) = options()?;
    match command.as_str() {
        "serve" => {
            let credentials = PathBuf::from(required(&opts, "--credentials")?);
            anyhow::ensure!(
                !Credentials::load(&credentials)?.clients.is_empty(),
                "Pair a client before enabling remote access"
            );
            let listener = tokio::net::TcpListener::bind(required(&opts, "--bind")?).await?;
            let options = ServerOptions::new(required(&opts, "--upstream")?, credentials);
            println!(
                "Noches remote access listening on {}",
                listener.local_addr()?
            );
            remote::serve(listener, options).await?;
        }
        "pair" => {
            let path = PathBuf::from(required(&opts, "--credentials")?);
            let upstream = required(&opts, "--upstream")?;
            let engine = zeron_rpc::connect_ws(&upstream).await?;
            let info = tokio::time::timeout(
                Duration::from_secs(8),
                engine.call(methods::ENGINE_INFO, serde_json::json!({})),
            )
            .await??;
            let profile = ConnectionProfile {
                id: uuid::Uuid::new_v4().to_string(),
                name: required(&opts, "--name")?,
                endpoint: required(&opts, "--endpoint")?,
                token: format!(
                    "{}{}",
                    uuid::Uuid::new_v4().simple(),
                    uuid::Uuid::new_v4().simple()
                ),
                device_id: info["deviceId"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("Missing engine identity"))?
                    .into(),
            };
            let code = profile.code()?;
            let mut credentials = Credentials::load(&path)?;
            credentials.clients.push(profile.clone());
            credentials.save(&path)?;
            remote::private_write(&PathBuf::from(required(&opts, "--out")?), code.as_bytes())?;
            println!(
                "Paired client {}. Connection code saved to the requested private file.",
                profile.id
            );
        }
        "revoke" => {
            let path = PathBuf::from(required(&opts, "--credentials")?);
            let id = required(&opts, "--id")?;
            let mut credentials = Credentials::load(&path)?;
            let before = credentials.clients.len();
            credentials.clients.retain(|c| c.id != id);
            anyhow::ensure!(credentials.clients.len() < before, "Client not found");
            credentials.save(&path)?;
            println!("Client revoked. Active sessions will close within five seconds.");
        }
        "import" => {
            let code = std::fs::read_to_string(required(&opts, "--code-file")?)?;
            let profile = ConnectionProfile::from_code(&code)?;
            let dir = PathBuf::from(required(&opts, "--data-dir")?);
            let mut connections = Connections::load(&dir)?;
            println!("Saving connection to {}", profile.name);
            connections.add(profile);
            connections.save(&dir)?;
        }
        "probe" => {
            let code = std::fs::read_to_string(required(&opts, "--code-file")?)?;
            let profile = ConnectionProfile::from_code(&code)?;
            let remote = remote::connect(profile.clone())?;
            let info = tokio::time::timeout(
                Duration::from_secs(20),
                remote
                    .client
                    .call(methods::ENGINE_INFO, serde_json::json!({})),
            )
            .await??;
            anyhow::ensure!(
                info["deviceId"].as_str() == Some(&profile.device_id),
                "Unexpected host identity"
            );
            let mut counts = serde_json::Map::new();
            for method in [
                methods::WATCH_CHATS,
                methods::WATCH_SPACES,
                methods::WATCH_DEVICES,
            ] {
                let mut stream = remote
                    .client
                    .subscribe(method, serde_json::json!({}))
                    .await?;
                let snapshot = tokio::time::timeout(Duration::from_secs(10), stream.recv())
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("Stream ended"))?;
                counts.insert(
                    method.into(),
                    serde_json::json!(snapshot.as_array().map(Vec::len)),
                );
            }
            println!(
                "{}",
                serde_json::json!({"host":profile.name,"identityVerified":true,"scope":info["workspaceScope"],"rows":counts})
            );
            remote.shutdown().await;
        }
        _ => println!(
            "Noches native connections\n\nserve --bind TAILNET_IP:PORT --upstream ws://127.0.0.1:ENGINE_PORT --credentials FILE\npair --name COMPUTER --endpoint ws://TAILNET_IP:PORT --upstream ws://127.0.0.1:ENGINE_PORT --credentials FILE --out PRIVATE_CODE_FILE\nrevoke --credentials FILE --id CLIENT_ID\nimport --code-file FILE --data-dir APP_DATA_DIR\nprobe --code-file FILE\n\nPaste the private connection code in Noches Settings > Connections."
        ),
    }
    Ok(())
}
