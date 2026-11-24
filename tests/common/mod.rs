#![allow(dead_code)]

use {
    solana_instruction_error::InstructionError,
    solana_program_test::BanksClientError,
    solana_transaction_error::{TransactionError, TransportError},
    spl_token_2022_interface::error::TokenError,
    spl_token_client::token::TokenError as ClientError,
};

/// Wire size of a transaction carrying `instructions`, with one
/// placeholder signature per required signer.
pub fn wire_size(
    instructions: &[solana_instruction::Instruction],
    payer: &solana_pubkey::Pubkey,
) -> usize {
    let tx = solana_transaction::Transaction::new_with_payer(instructions, Some(payer));
    bincode::serialized_size(&tx).unwrap() as usize
}

/// The instruction error inside a failed client call, if it reached the chain.
pub fn instruction_error(err: &ClientError) -> Option<InstructionError> {
    match err {
        ClientError::Client(inner) => {
            let tx_error = if let Some(e) = inner.downcast_ref::<BanksClientError>() {
                match e {
                    BanksClientError::TransactionError(e) => Some(e.clone()),
                    BanksClientError::SimulationError { err, .. } => Some(err.clone()),
                    _ => None,
                }
            } else if let Some(TransportError::TransactionError(e)) =
                inner.downcast_ref::<TransportError>()
            {
                Some(e.clone())
            } else {
                None
            };
            match tx_error {
                Some(TransactionError::InstructionError(_, e)) => Some(e),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Asserts that a call was rejected on chain by Token-2022 with `expected`.
#[track_caller]
pub fn assert_token_error<T: std::fmt::Debug>(
    result: Result<T, ClientError>,
    expected: TokenError,
) {
    let err = result.expect_err("expected the transaction to fail");
    assert_eq!(
        instruction_error(&err),
        Some(InstructionError::Custom(expected.clone() as u32)),
        "expected {expected:?}, got {err:?}"
    );
}

/// Asserts that a call was rejected on chain with a non-custom error.
#[track_caller]
pub fn assert_instruction_error<T: std::fmt::Debug>(
    result: Result<T, ClientError>,
    expected: InstructionError,
) {
    let err = result.expect_err("expected the transaction to fail");
    assert_eq!(instruction_error(&err), Some(expected), "got {err:?}");
}

use {
    remit_token22::{
        confidential::{self, ConfidentialAccount, TransferKeys},
        remit::{self, RemitV2},
    },
    solana_keypair::Keypair,
    solana_signer::Signer,
    solana_zk_sdk::encryption::elgamal::ElGamalPubkey,
};

/// Open account (frozen) -> KYC thaw -> owner configures confidential keys
/// -> confidential authority approves.
pub async fn onboard_confidential(remit: &RemitV2, owner: &Keypair) -> ConfidentialAccount {
    let account = remit.open_account(&owner.pubkey()).await.unwrap();
    remit.kyc_approve(&account).await.unwrap();
    let account = confidential::configure(&remit.token, &account, owner)
        .await
        .unwrap();
    remit.approve_confidential(&account.address).await.unwrap();
    account
}

/// Mint `amount` publicly, deposit it and apply it, so it all sits in the
/// available confidential balance.
pub async fn fund_confidential(
    remit: &RemitV2,
    account: &ConfidentialAccount,
    owner: &Keypair,
    amount: u64,
) {
    remit.mint_to(&account.address, amount).await.unwrap();
    confidential::deposit(
        &remit.token,
        account,
        owner,
        amount,
        remit_token22::remit::DECIMALS,
    )
    .await
    .unwrap();
    confidential::apply_pending(&remit.token, account, owner)
        .await
        .unwrap();
}

/// Keys and fee parameters for a confidential transfer. The fee parameters
/// are read from the mint for the current epoch, not taken from constants.
pub async fn transfer_keys<'a>(
    remit: &'a RemitV2,
    destination: &'a ElGamalPubkey,
) -> TransferKeys<'a> {
    let (fee_basis_points, maximum_fee) = remit::current_fee_parameters(&remit.env, &remit.token)
        .await
        .unwrap();
    TransferKeys {
        destination,
        auditor: remit.confidential.auditor.pubkey(),
        withdraw_withheld_authority: remit.confidential.withdraw_withheld_elgamal.pubkey(),
        fee_basis_points,
        maximum_fee,
    }
}
