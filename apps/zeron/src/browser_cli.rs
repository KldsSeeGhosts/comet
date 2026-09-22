use base64::Engine;
use std::{
    io::{BufRead, Write},
    path::Path,
};
use zeron_browser::{Action, Request, transport};

pub fn command(
    socket: &Path,
    session: &str,
    output: Option<&Path>,
    request: &str,
) -> anyhow::Result<()> {
    let action: Action = serde_json::from_str(request)?;
    if output.is_some() && !matches!(action, Action::Screenshot { .. }) {
        anyhow::bail!("--output requires the screenshot action");
    }
    let result = transport::connect(
        socket,
        &Request {
            session: session.into(),
            action,
        },
    )
    .map_err(anyhow::Error::msg)?;
    if let Some(path) = output {
        let png = result["png"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Browser returned no screenshot"))?;
        let bytes = base64::engine::general_purpose::STANDARD.decode(png)?;
        if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            anyhow::bail!("Browser returned an invalid PNG");
        }
        std::fs::write(path, bytes)?;
        println!(
            "{}",
            serde_json::json!({"path":path,"mimeType":"image/png"})
        );
    } else {
        println!("{result}");
    }
    Ok(())
}
pub fn mcp(socket: &Path, session: &str) -> anyhow::Result<()> {
    let mut input = std::io::stdin().lock();
    let mut output = std::io::stdout().lock();
    loop {
        if input.fill_buf()?.is_empty() {
            break;
        }
        let bytes = transport::read_line(&mut input, transport::MAX_REQUEST)?;
        let response = match serde_json::from_slice(&bytes) {
            Ok(message) => zeron_browser::mcp::respond(message, session, |request| {
                transport::connect(socket, &request)
            }),
            Err(error) => Some(
                serde_json::json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":error.to_string()}}),
            ),
        };
        if let Some(response) = response {
            serde_json::to_writer(&mut output, &response)?;
            output.write_all(b"\n")?;
            output.flush()?;
        }
    }
    Ok(())
}
