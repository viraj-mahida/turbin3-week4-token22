//! Remit agent: the automation pattern on Token-2022.
//!
//! A user `Approve`s the agent PDA of an operator as delegate on their token
//! account. The operator can then move up to the approved amount on the
//! user's behalf; the program signs the CPI for its PDA. The user never signs
//! the agent's transfers.
//!
//! Instructions (first byte is the tag, then little-endian fields):
//!
//! * `0` DelegatedTransfer `{ amount: u64, decimals: u8, fee: u64 }`
//!   accounts: `[source (w), mint, destination (w), agent PDA, operator (s), token program]`
//!   CPI `TransferCheckedWithFee` with the agent PDA as authority
//!   (PDA seeds: `["agent", operator]`).
//! * `1` OwnerTransfer `{ amount: u64, decimals: u8, fee: u64 }`
//!   accounts: `[source (w), mint, destination (w), owner (s), token program]`
//!   CPI `TransferCheckedWithFee` with the account owner as authority. Used
//!   to contrast with the delegated path once CPI Guard is on.

use {
    solana_account_info::AccountInfo,
    solana_cpi::{invoke, invoke_signed},
    solana_instruction::{AccountMeta, Instruction},
    solana_msg::msg,
    solana_program_error::{ProgramError, ProgramResult},
    solana_pubkey::{pubkey, Pubkey},
};

/// Token-2022 program id.
pub const TOKEN_2022: Pubkey = pubkey!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
pub const AGENT_SEED: &[u8] = b"agent";

/// Token-2022 `TransferFeeExtension` tag and its `TransferCheckedWithFee` tag.
const TRANSFER_FEE_EXTENSION: u8 = 26;
const TRANSFER_CHECKED_WITH_FEE: u8 = 1;

#[cfg(not(feature = "no-entrypoint"))]
solana_program_entrypoint::entrypoint!(process_instruction);

pub fn process_instruction(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &[u8],
) -> ProgramResult {
    let (&tag, rest) = data
        .split_first()
        .ok_or(ProgramError::InvalidInstructionData)?;
    let args = TransferArgs::unpack(rest)?;
    match tag {
        0 => delegated_transfer(program_id, accounts, args),
        1 => owner_transfer(accounts, args),
        _ => Err(ProgramError::InvalidInstructionData),
    }
}

struct TransferArgs {
    amount: u64,
    decimals: u8,
    fee: u64,
}

impl TransferArgs {
    fn unpack(data: &[u8]) -> Result<Self, ProgramError> {
        if data.len() != 17 {
            return Err(ProgramError::InvalidInstructionData);
        }
        Ok(Self {
            amount: u64::from_le_bytes(data[0..8].try_into().unwrap()),
            decimals: data[8],
            fee: u64::from_le_bytes(data[9..17].try_into().unwrap()),
        })
    }
}

fn transfer_checked_with_fee(
    source: &AccountInfo,
    mint: &AccountInfo,
    destination: &AccountInfo,
    authority: &AccountInfo,
    args: &TransferArgs,
) -> Instruction {
    let mut data = Vec::with_capacity(19);
    data.push(TRANSFER_FEE_EXTENSION);
    data.push(TRANSFER_CHECKED_WITH_FEE);
    data.extend_from_slice(&args.amount.to_le_bytes());
    data.push(args.decimals);
    data.extend_from_slice(&args.fee.to_le_bytes());
    Instruction {
        program_id: TOKEN_2022,
        accounts: vec![
            AccountMeta::new(*source.key, false),
            AccountMeta::new_readonly(*mint.key, false),
            AccountMeta::new(*destination.key, false),
            AccountMeta::new_readonly(*authority.key, true),
        ],
        data,
    }
}

fn check_token_program(token_program: &AccountInfo) -> ProgramResult {
    if *token_program.key != TOKEN_2022 {
        return Err(ProgramError::IncorrectProgramId);
    }
    Ok(())
}

fn delegated_transfer(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    args: TransferArgs,
) -> ProgramResult {
    let [source, mint, destination, agent, operator, token_program] = accounts else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    check_token_program(token_program)?;
    if !operator.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    let (expected, bump) =
        Pubkey::find_program_address(&[AGENT_SEED, operator.key.as_ref()], program_id);
    if *agent.key != expected {
        return Err(ProgramError::InvalidSeeds);
    }
    msg!("remit-agent: delegated transfer of {}", args.amount);
    let ix = transfer_checked_with_fee(source, mint, destination, agent, &args);
    invoke_signed(
        &ix,
        &[
            source.clone(),
            mint.clone(),
            destination.clone(),
            agent.clone(),
        ],
        &[&[AGENT_SEED, operator.key.as_ref(), &[bump]]],
    )
}

fn owner_transfer(accounts: &[AccountInfo], args: TransferArgs) -> ProgramResult {
    let [source, mint, destination, owner, token_program] = accounts else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    check_token_program(token_program)?;
    msg!("remit-agent: owner-signed transfer of {}", args.amount);
    let ix = transfer_checked_with_fee(source, mint, destination, owner, &args);
    invoke(
        &ix,
        &[
            source.clone(),
            mint.clone(),
            destination.clone(),
            owner.clone(),
        ],
    )
}
