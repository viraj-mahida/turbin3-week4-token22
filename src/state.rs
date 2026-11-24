//! All account and mint state in this repo is decoded here, and only through
//! `StateWithExtensions::<Mint>::unpack` / `StateWithExtensions::<Account>::unpack`.
//! Nothing uses the raw `Pack::unpack` of the base state, which would reject
//! (or misread) accounts that carry extensions.

use spl_token_2022_interface::{
    extension::StateWithExtensions,
    state::{Account, Mint},
};

/// Account bytes as fetched from the bank.
pub struct RawAccount {
    pub lamports: u64,
    pub data: Vec<u8>,
}

impl RawAccount {
    /// Mint base state plus its extensions.
    pub fn mint(&self) -> StateWithExtensions<'_, Mint> {
        StateWithExtensions::<Mint>::unpack(&self.data).expect("not a Token-2022 mint")
    }

    /// Token account base state plus its extensions.
    pub fn token_account(&self) -> StateWithExtensions<'_, Account> {
        StateWithExtensions::<Account>::unpack(&self.data).expect("not a Token-2022 account")
    }
}

use {crate::env::RemitToken, solana_pubkey::Pubkey, spl_token_client::token::TokenResult};

/// Fetches the raw bytes of any account through the token client.
pub async fn read(token: &RemitToken, address: &Pubkey) -> TokenResult<RawAccount> {
    let account = token.get_account(*address).await?;
    Ok(RawAccount {
        lamports: account.lamports,
        data: account.data,
    })
}

/// Public `amount` of a token account.
pub async fn amount(token: &RemitToken, address: &Pubkey) -> u64 {
    read(token, address)
        .await
        .unwrap()
        .token_account()
        .base
        .amount
}

/// Current supply of the token's mint.
pub async fn supply(token: &RemitToken) -> u64 {
    read(token, token.get_address())
        .await
        .unwrap()
        .mint()
        .base
        .supply
}
