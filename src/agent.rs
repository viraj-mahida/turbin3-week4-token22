//! Client side of the `remit-agent` on-chain program (`programs/remit-agent`).

use {
    solana_instruction::{AccountMeta, Instruction},
    solana_pubkey::{pubkey, Pubkey},
};

/// Address the tests load `tests/fixtures/remit_agent.so` at.
pub const ID: Pubkey = pubkey!("5okcUrmWCMCN7HRwGskdt93d6Q1npneDy6dpV92z9Gx4");
pub const AGENT_SEED: &[u8] = b"agent";

/// The PDA an operator's transfers are signed by. Users `Approve` this
/// address as delegate.
pub fn agent_pda(operator: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[AGENT_SEED, operator.as_ref()], &ID).0
}

fn data(tag: u8, amount: u64, decimals: u8, fee: u64) -> Vec<u8> {
    let mut data = vec![tag];
    data.extend_from_slice(&amount.to_le_bytes());
    data.push(decimals);
    data.extend_from_slice(&fee.to_le_bytes());
    data
}

/// Operator-triggered transfer out of `source`, authorised by the delegation
/// the owner gave to [`agent_pda`].
pub fn delegated_transfer(
    source: &Pubkey,
    mint: &Pubkey,
    destination: &Pubkey,
    operator: &Pubkey,
    amount: u64,
    decimals: u8,
    fee: u64,
) -> Instruction {
    Instruction {
        program_id: ID,
        accounts: vec![
            AccountMeta::new(*source, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new(*destination, false),
            AccountMeta::new_readonly(agent_pda(operator), false),
            AccountMeta::new_readonly(*operator, true),
            AccountMeta::new_readonly(spl_token_2022_interface::id(), false),
        ],
        data: data(0, amount, decimals, fee),
    }
}

/// The owner signs and the program forwards that signature in a CPI.
pub fn owner_transfer(
    source: &Pubkey,
    mint: &Pubkey,
    destination: &Pubkey,
    owner: &Pubkey,
    amount: u64,
    decimals: u8,
    fee: u64,
) -> Instruction {
    Instruction {
        program_id: ID,
        accounts: vec![
            AccountMeta::new(*source, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new(*destination, false),
            AccountMeta::new_readonly(*owner, true),
            AccountMeta::new_readonly(spl_token_2022_interface::id(), false),
        ],
        data: data(1, amount, decimals, fee),
    }
}
