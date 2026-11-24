//! Confidential transfer helpers: key derivation, account setup, and the
//! proof-heavy steps (transfer with fee, withdraw, fee withdrawal).
//!
//! Large proofs are verified into context state accounts first and the token
//! instruction then points at those accounts. That keeps every transaction
//! under the 1232-byte packet limit, the same way a client on a real cluster
//! has to do it.

use {
    crate::{env::RemitToken, state},
    solana_keypair::Keypair,
    solana_pubkey::Pubkey,
    solana_signer::{signers::Signers, Signer},
    solana_zk_sdk::encryption::{
        auth_encryption::AeKey,
        derivation::{derive_confidential_keys_from_signature, STANDARD_DERIVATION_MESSAGE},
        elgamal::{ElGamalCiphertext, ElGamalKeypair, ElGamalPubkey},
    },
    spl_token_2022_interface::extension::{
        confidential_transfer::ConfidentialTransferAccount,
        confidential_transfer_fee::ConfidentialTransferFeeAmount, BaseStateWithExtensions,
        ExtensionType,
    },
    spl_token_client::{
        token::{ProofAccountWithCiphertext, TokenResult},
        zk_proofs::{
            confidential_transfer::{
                ApplyPendingBalanceAccountInfo, TransferAccountInfo, WithdrawAccountInfo,
            },
            confidential_transfer_fee::WithheldTokensInfo,
        },
    },
    spl_token_confidential_transfer_proof_generation::{
        transfer_with_fee::TransferWithFeeProofData, withdraw::WithdrawProofData,
    },
};

/// A configured confidential account and the owner's decryption keys.
pub struct ConfidentialAccount {
    pub address: Pubkey,
    pub elgamal: ElGamalKeypair,
    pub aes: AeKey,
}

/// Decrypted view of the confidential part of an account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Balances {
    /// Public (non-confidential) balance, `Account::amount`.
    pub public: u64,
    /// Incoming confidential credits not yet applied.
    pub pending: u64,
    /// Spendable confidential balance.
    pub available: u64,
}

/// Derives the owner's ElGamal and AES keys from a signature over a fixed
/// message, the same scheme wallets use. Nobody without the owner's signing
/// key can derive them.
pub fn derive_keys(owner: &Keypair) -> (ElGamalKeypair, AeKey) {
    let signature = owner
        .try_sign_message(STANDARD_DERIVATION_MESSAGE)
        .expect("sign derivation message");
    derive_confidential_keys_from_signature(&signature).expect("derive confidential keys")
}

/// Adds the confidential extensions to an existing token account and
/// configures it with the owner's keys (includes a pubkey validity proof).
pub async fn configure(
    token: &RemitToken,
    account: &Pubkey,
    owner: &Keypair,
) -> TokenResult<ConfidentialAccount> {
    token
        .reallocate(
            account,
            &owner.pubkey(),
            &[
                ExtensionType::ConfidentialTransferAccount,
                ExtensionType::ConfidentialTransferFeeAmount,
            ],
            &[owner],
        )
        .await?;
    let (elgamal, aes) = derive_keys(owner);
    token
        .confidential_transfer_configure_token_account(
            account,
            &owner.pubkey(),
            None,
            None,
            &elgamal,
            &aes,
            &[owner],
        )
        .await?;
    Ok(ConfidentialAccount {
        address: *account,
        elgamal,
        aes,
    })
}

/// Public balance -> pending confidential balance.
pub async fn deposit(
    token: &RemitToken,
    account: &ConfidentialAccount,
    owner: &Keypair,
    amount: u64,
    decimals: u8,
) -> TokenResult<()> {
    token
        .confidential_transfer_deposit(
            &account.address,
            &owner.pubkey(),
            amount,
            decimals,
            &[owner],
        )
        .await?;
    Ok(())
}

/// Pending -> available. The owner decrypts the pending balance and writes a
/// fresh AES-encrypted copy of the new available balance.
pub async fn apply_pending(
    token: &RemitToken,
    account: &ConfidentialAccount,
    owner: &Keypair,
) -> TokenResult<()> {
    let info = ApplyPendingBalanceAccountInfo::new(&raw_state(token, &account.address).await?);
    token
        .confidential_transfer_apply_pending_balance(
            &account.address,
            &owner.pubkey(),
            Some(info),
            account.elgamal.secret(),
            &account.aes,
            &[owner],
        )
        .await?;
    Ok(())
}

pub async fn balances(token: &RemitToken, account: &ConfidentialAccount) -> TokenResult<Balances> {
    let raw = state::read(token, &account.address).await?;
    let state = raw.token_account();
    let ct = state.get_extension::<ConfidentialTransferAccount>()?;
    let decrypt = |c: &solana_zk_sdk_pod::encryption::elgamal::PodElGamalCiphertext| {
        let c: ElGamalCiphertext = (*c)
            .try_into()
            .map_err(|_| spl_token_client::token::TokenError::AccountDecryption)?;
        account
            .elgamal
            .secret()
            .decrypt_u32(&c)
            .ok_or(spl_token_client::token::TokenError::AccountDecryption)
    };
    let pending = decrypt(&ct.pending_balance_lo)? + (decrypt(&ct.pending_balance_hi)? << 16);
    let decryptable: solana_zk_sdk::encryption::auth_encryption::AeCiphertext = ct
        .decryptable_available_balance
        .try_into()
        .map_err(|_| spl_token_client::token::TokenError::AccountDecryption)?;
    let available = account
        .aes
        .decrypt(&decryptable)
        .ok_or(spl_token_client::token::TokenError::AccountDecryption)?;
    Ok(Balances {
        public: state.base.amount,
        pending,
        available,
    })
}

/// Raw confidential state of an account, as anyone on chain can read it.
pub async fn raw_state(
    token: &RemitToken,
    account: &Pubkey,
) -> TokenResult<ConfidentialTransferAccount> {
    let raw = state::read(token, account).await?;
    let ct = *raw
        .token_account()
        .get_extension::<ConfidentialTransferAccount>()?;
    Ok(ct)
}

/// Fee parameters and recipients a confidential transfer has to prove against.
pub struct TransferKeys<'a> {
    pub destination: &'a ElGamalPubkey,
    pub auditor: &'a ElGamalPubkey,
    pub withdraw_withheld_authority: &'a ElGamalPubkey,
    pub fee_basis_points: u16,
    pub maximum_fee: u64,
}

/// What the auditor gets from a confidential transfer: the amount encrypted
/// under the auditor key, split into 16 low bits and 32 high bits.
pub struct AuditorCiphertexts {
    pub lo: ElGamalCiphertext,
    pub hi: ElGamalCiphertext,
}

impl AuditorCiphertexts {
    pub fn decrypt(&self, auditor: &ElGamalKeypair) -> Option<u64> {
        let lo = auditor.secret().decrypt_u32(&self.lo)?;
        let hi = auditor.secret().decrypt_u32(&self.hi)?;
        Some(lo + (hi << 16))
    }
}

/// Confidential transfer on a mint with a transfer fee.
///
/// The five proofs (equality, amount validity, fee percentage, fee validity,
/// range) are generated from the source account's keys, verified into context
/// state accounts, used by the transfer, then closed to recover rent.
/// `authority`/`signers` are separate from `source` so the tests can try
/// other signers with otherwise valid proofs.
#[allow(clippy::too_many_arguments)]
pub async fn transfer_with_fee<S: Signers>(
    token: &RemitToken,
    payer: &Pubkey,
    source: &ConfidentialAccount,
    destination: &Pubkey,
    authority: &Pubkey,
    signers: &S,
    amount: u64,
    keys: &TransferKeys<'_>,
) -> TokenResult<AuditorCiphertexts> {
    let state = raw_state(token, &source.address).await?;
    let TransferWithFeeProofData {
        equality_proof_data,
        transfer_amount_ciphertext_validity_proof_data_with_ciphertext: validity,
        percentage_with_cap_proof_data,
        fee_ciphertext_validity_proof_data,
        range_proof_data,
    } = TransferAccountInfo::new(&state)
        .generate_split_transfer_with_fee_proof_data(
            amount,
            &source.elgamal,
            &source.aes,
            keys.destination,
            Some(keys.auditor),
            keys.withdraw_withheld_authority,
            keys.fee_basis_points,
            keys.maximum_fee,
        )
        .map_err(|_| spl_token_client::token::TokenError::ProofGeneration)?;

    let context_authority = Keypair::new();
    let equality = Keypair::new();
    let amount_validity = Keypair::new();
    let percentage = Keypair::new();
    let fee_validity = Keypair::new();
    let range = Keypair::new();
    let ca = context_authority.pubkey();

    token
        .confidential_transfer_create_context_state_account(
            &equality.pubkey(),
            &ca,
            &equality_proof_data,
            &[&equality],
        )
        .await?;
    token
        .confidential_transfer_create_context_state_account(
            &amount_validity.pubkey(),
            &ca,
            &validity.proof_data,
            &[&amount_validity],
        )
        .await?;
    token
        .confidential_transfer_create_context_state_account(
            &percentage.pubkey(),
            &ca,
            &percentage_with_cap_proof_data,
            &[&percentage],
        )
        .await?;
    token
        .confidential_transfer_create_context_state_account(
            &fee_validity.pubkey(),
            &ca,
            &fee_ciphertext_validity_proof_data,
            &[&fee_validity],
        )
        .await?;
    token
        .confidential_transfer_create_context_state_account(
            &range.pubkey(),
            &ca,
            &range_proof_data,
            &[&range],
        )
        .await?;

    let auditor = AuditorCiphertexts {
        lo: validity
            .ciphertext_lo
            .try_into()
            .expect("auditor ciphertext lo"),
        hi: validity
            .ciphertext_hi
            .try_into()
            .expect("auditor ciphertext hi"),
    };
    let amount_validity_with_ciphertext = ProofAccountWithCiphertext {
        context_state_account: amount_validity.pubkey(),
        ciphertext_lo: validity.ciphertext_lo,
        ciphertext_hi: validity.ciphertext_hi,
    };

    let result = token
        .confidential_transfer_transfer_with_fee(
            &source.address,
            destination,
            authority,
            Some(&equality.pubkey()),
            Some(&amount_validity_with_ciphertext),
            Some(&percentage.pubkey()),
            Some(&fee_validity.pubkey()),
            Some(&range.pubkey()),
            amount,
            Some(TransferAccountInfo::new(&state)),
            &source.elgamal,
            &source.aes,
            keys.destination,
            Some(keys.auditor),
            keys.withdraw_withheld_authority,
            keys.fee_basis_points,
            keys.maximum_fee,
            signers,
        )
        .await;

    for proof in [
        &equality,
        &amount_validity,
        &percentage,
        &fee_validity,
        &range,
    ] {
        token
            .confidential_transfer_close_context_state_account(
                &proof.pubkey(),
                payer,
                &ca,
                &[&context_authority],
            )
            .await?;
    }
    result.map(|_| auditor)
}

/// Available confidential balance -> public balance (equality + range proof).
pub async fn withdraw<S: Signers>(
    token: &RemitToken,
    payer: &Pubkey,
    account: &ConfidentialAccount,
    authority: &Pubkey,
    signers: &S,
    amount: u64,
    decimals: u8,
) -> TokenResult<()> {
    let state = raw_state(token, &account.address).await?;
    withdraw_against(
        token,
        payer,
        account,
        authority,
        signers,
        amount,
        decimals,
        WithdrawAccountInfo::new(&state),
    )
    .await
}

/// Withdraw with proofs built against `info` (the balance the client believes
/// is available). [`withdraw`] passes the account's real available balance;
/// tests pass other ciphertexts to show the program checks them.
#[allow(clippy::too_many_arguments)]
pub async fn withdraw_against<S: Signers>(
    token: &RemitToken,
    payer: &Pubkey,
    account: &ConfidentialAccount,
    authority: &Pubkey,
    signers: &S,
    amount: u64,
    decimals: u8,
    info: WithdrawAccountInfo,
) -> TokenResult<()> {
    let WithdrawProofData {
        equality_proof_data,
        range_proof_data,
    } = info
        .generate_proof_data(amount, &account.elgamal, &account.aes)
        .map_err(|_| spl_token_client::token::TokenError::ProofGeneration)?;

    let context_authority = Keypair::new();
    let equality = Keypair::new();
    let range = Keypair::new();
    let ca = context_authority.pubkey();
    token
        .confidential_transfer_create_context_state_account(
            &equality.pubkey(),
            &ca,
            &equality_proof_data,
            &[&equality],
        )
        .await?;
    token
        .confidential_transfer_create_context_state_account(
            &range.pubkey(),
            &ca,
            &range_proof_data,
            &[&range],
        )
        .await?;

    let result = token
        .confidential_transfer_withdraw(
            &account.address,
            authority,
            Some(&equality.pubkey()),
            Some(&range.pubkey()),
            amount,
            decimals,
            Some(info),
            &account.elgamal,
            &account.aes,
            signers,
        )
        .await;

    for proof in [&equality, &range] {
        token
            .confidential_transfer_close_context_state_account(
                &proof.pubkey(),
                payer,
                &ca,
                &[&context_authority],
            )
            .await?;
    }
    result.map(|_| ())
}

/// Encrypted fee withheld in an account by confidential transfers, decrypted
/// with the withdraw-withheld authority's ElGamal key.
pub async fn withheld_confidential_fee(
    token: &RemitToken,
    account: &Pubkey,
    withdraw_withheld_elgamal: &ElGamalKeypair,
) -> TokenResult<u64> {
    let raw = state::read(token, account).await?;
    let state = raw.token_account();
    let extension = state.get_extension::<ConfidentialTransferFeeAmount>()?;
    let ciphertext: ElGamalCiphertext = extension
        .withheld_amount
        .try_into()
        .map_err(|_| spl_token_client::token::TokenError::AccountDecryption)?;
    withdraw_withheld_elgamal
        .secret()
        .decrypt_u32(&ciphertext)
        .ok_or(spl_token_client::token::TokenError::AccountDecryption)
}

/// Fee desk moves confidentially withheld fees from `sources` into its own
/// confidential account (one equality proof, re-encrypting the fee from the
/// withdraw-withheld key to the destination key).
pub async fn withdraw_withheld_confidential_fees(
    token: &RemitToken,
    destination: &ConfidentialAccount,
    authority: &Keypair,
    withdraw_withheld_elgamal: &ElGamalKeypair,
    source: &Pubkey,
    new_destination_available: u64,
) -> TokenResult<()> {
    let raw = state::read(token, source).await?;
    let state = raw.token_account();
    let extension = state.get_extension::<ConfidentialTransferFeeAmount>()?;
    let info = WithheldTokensInfo::new(&extension.withheld_amount);
    let new_decryptable = destination.aes.encrypt(new_destination_available);
    token
        .confidential_transfer_withdraw_withheld_tokens_from_accounts(
            &destination.address,
            &authority.pubkey(),
            None,
            Some(info),
            withdraw_withheld_elgamal,
            destination.elgamal.pubkey(),
            &new_decryptable.into(),
            &[source],
            &[authority],
        )
        .await?;
    Ok(())
}
