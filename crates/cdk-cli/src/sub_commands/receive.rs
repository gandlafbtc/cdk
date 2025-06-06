use std::collections::HashSet;
use std::path::Path;
use std::str::FromStr;

use anyhow::{anyhow, Result};
use cdk::nuts::{SecretKey, Token};
use cdk::util::unix_time;
use cdk::wallet::multi_mint_wallet::MultiMintWallet;
use cdk::wallet::types::WalletKey;
use cdk::wallet::ReceiveOptions;
use cdk::Amount;
use clap::Args;
use nostr_sdk::nips::nip04;
use nostr_sdk::{Filter, Keys, Kind, Timestamp};

use crate::nostr_storage;
use crate::utils::get_or_create_wallet;

#[derive(Args)]
pub struct ReceiveSubCommand {
    /// Cashu Token
    token: Option<String>,
    /// Signing Key
    #[arg(short, long, action = clap::ArgAction::Append)]
    signing_key: Vec<String>,
    /// Nostr key
    #[arg(short, long)]
    nostr_key: Option<String>,
    /// Nostr relay
    #[arg(short, long, action = clap::ArgAction::Append)]
    relay: Vec<String>,
    /// Unix time to query nostr from
    #[arg(long)]
    since: Option<u64>,
    /// Preimage
    #[arg(short, long,  action = clap::ArgAction::Append)]
    preimage: Vec<String>,
}

pub async fn receive(
    multi_mint_wallet: &MultiMintWallet,
    sub_command_args: &ReceiveSubCommand,
    work_dir: &Path,
) -> Result<()> {
    let mut signing_keys = Vec::new();

    if !sub_command_args.signing_key.is_empty() {
        let mut s_keys: Vec<SecretKey> = sub_command_args
            .signing_key
            .iter()
            .map(|s| {
                if s.starts_with("nsec") {
                    let nostr_key = nostr_sdk::SecretKey::from_str(s).expect("Invalid secret key");

                    SecretKey::from_str(&nostr_key.to_secret_hex())
                } else {
                    SecretKey::from_str(s)
                }
            })
            .collect::<Result<Vec<SecretKey>, _>>()?;
        signing_keys.append(&mut s_keys);
    }

    let amount = match &sub_command_args.token {
        Some(token_str) => {
            receive_token(
                multi_mint_wallet,
                token_str,
                &signing_keys,
                &sub_command_args.preimage,
            )
            .await?
        }
        None => {
            //wallet.add_p2pk_signing_key(nostr_signing_key).await;
            let nostr_key = match sub_command_args.nostr_key.as_ref() {
                Some(nostr_key) => {
                    let secret_key = nostr_sdk::SecretKey::from_str(nostr_key)?;
                    let secret_key = SecretKey::from_str(&secret_key.to_secret_hex())?;
                    Some(secret_key)
                }
                None => None,
            };

            let nostr_key =
                nostr_key.ok_or(anyhow!("Nostr key required if token is not provided"))?;

            signing_keys.push(nostr_key.clone());

            let relays = sub_command_args.relay.clone();
            let since =
                nostr_storage::get_nostr_last_checked(work_dir, &nostr_key.public_key()).await?;

            let tokens = nostr_receive(relays, nostr_key.clone(), since).await?;

            // Store the current time as last checked
            nostr_storage::store_nostr_last_checked(
                work_dir,
                &nostr_key.public_key(),
                unix_time() as u32,
            )
            .await?;

            let mut total_amount = Amount::ZERO;
            for token_str in &tokens {
                match receive_token(
                    multi_mint_wallet,
                    token_str,
                    &signing_keys,
                    &sub_command_args.preimage,
                )
                .await
                {
                    Ok(amount) => {
                        total_amount += amount;
                    }
                    Err(err) => {
                        println!("{err}");
                    }
                }
            }

            total_amount
        }
    };

    println!("Received: {amount}");

    Ok(())
}

async fn receive_token(
    multi_mint_wallet: &MultiMintWallet,
    token_str: &str,
    signing_keys: &[SecretKey],
    preimage: &[String],
) -> Result<Amount> {
    let token: Token = Token::from_str(token_str)?;

    let mint_url = token.mint_url()?;
    let unit = token.unit().unwrap_or_default();

    if multi_mint_wallet
        .get_wallet(&WalletKey::new(mint_url.clone(), unit.clone()))
        .await
        .is_none()
    {
        get_or_create_wallet(multi_mint_wallet, &mint_url, unit).await?;
    }

    let amount = multi_mint_wallet
        .receive(
            token_str,
            ReceiveOptions {
                p2pk_signing_keys: signing_keys.to_vec(),
                preimages: preimage.to_vec(),
                ..Default::default()
            },
        )
        .await?;
    Ok(amount)
}

/// Receive tokens sent to nostr pubkey via dm
async fn nostr_receive(
    relays: Vec<String>,
    nostr_signing_key: SecretKey,
    since: Option<u32>,
) -> Result<HashSet<String>> {
    let verifying_key = nostr_signing_key.public_key();

    let x_only_pubkey = verifying_key.x_only_public_key();

    let nostr_pubkey = nostr_sdk::PublicKey::from_hex(x_only_pubkey.to_string())?;

    let since = since.map(|s| Timestamp::from(s as u64));

    let filter = match since {
        Some(since) => Filter::new()
            .pubkey(nostr_pubkey)
            .kind(Kind::EncryptedDirectMessage)
            .since(since),
        None => Filter::new()
            .pubkey(nostr_pubkey)
            .kind(Kind::EncryptedDirectMessage),
    };

    let client = nostr_sdk::Client::default();

    client.connect().await;

    let events = client
        .get_events_of(
            vec![filter],
            nostr_sdk::EventSource::Relays {
                timeout: None,
                specific_relays: Some(relays),
            },
        )
        .await?;

    let mut tokens: HashSet<String> = HashSet::new();

    let keys = Keys::from_str(&(nostr_signing_key).to_secret_hex())?;

    for event in events {
        if event.kind == Kind::EncryptedDirectMessage {
            if let Ok(msg) = nip04::decrypt(keys.secret_key(), &event.pubkey, event.content) {
                if let Some(token) = cdk::wallet::util::token_from_text(&msg) {
                    tokens.insert(token.to_string());
                }
            } else {
                tracing::error!("Impossible to decrypt direct message");
            }
        }
    }

    Ok(tokens)
}
