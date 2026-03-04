use anyhow::{anyhow, Result};
use greentic_secrets_lib::{DevStore, SecretsStore};
use std::env;

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage: read_secret <store_path> <secret_uri>");
        eprintln!("Example: read_secret demo-bundle/.greentic/dev/.dev.secrets.env secrets://dev/default/_/messaging-telegram/telegram_bot_token");
        std::process::exit(1);
    }
    let store_path = &args[1];
    let uri = &args[2];

    let store = DevStore::with_path(store_path)
        .map_err(|err| anyhow!("failed to open store {store_path}: {err}"))?;
    let value = store
        .get(uri)
        .await
        .map_err(|err| anyhow!("failed to read {uri}: {err}"))?;
    let text = String::from_utf8(value).map_err(|_| anyhow!("secret is not valid UTF-8"))?;
    print!("{text}");
    Ok(())
}
