//! Extension challenge: delegated automation through a program, under CPI
//! Guard.
//!
//! `programs/remit-agent` is loaded from `tests/fixtures/remit_agent.so`. A
//! user approves the agent PDA as delegate; an operator then triggers
//! transfers that the program signs for its PDA in a CPI into Token-2022.
//! After the user turns on CPI Guard, the delegated path keeps working
//! unchanged, while an owner-signed transfer through the same program is
//! blocked.

mod common;

use {
    common::{assert_instruction_error, assert_token_error},
    remit_token22::{
        agent,
        remit::{self, RemitV1, DECIMALS, ONE},
        state, Env,
    },
    solana_instruction::{AccountMeta, Instruction},
    solana_instruction_error::InstructionError,
    solana_keypair::Keypair,
    solana_pubkey::Pubkey,
    solana_signer::Signer,
    spl_token_2022_interface::{
        error::TokenError,
        extension::{cpi_guard::CpiGuard, BaseStateWithExtensions, ExtensionType},
    },
};

struct Scene {
    env: Env,
    remit: RemitV1,
    user: Keypair,
    source: Pubkey,
    payee: Pubkey,
    operator: Keypair,
}

/// A KYC'd user holding 1,000 RMTD who approved the operator's agent PDA
/// for 300 RMTD, and a KYC'd payee.
async fn scene() -> Scene {
    let env = Env::start().await;
    let remit = RemitV1::create(&env).await.unwrap();
    let (user, payee_owner, operator) = (Keypair::new(), Keypair::new(), Keypair::new());
    let source = remit.open_account(&user.pubkey()).await.unwrap();
    remit.kyc_approve(&source).await.unwrap();
    let payee = remit.open_account(&payee_owner.pubkey()).await.unwrap();
    remit.kyc_approve(&payee).await.unwrap();
    remit.mint_to(&source, 1_000 * ONE).await.unwrap();

    remit
        .token
        .approve(
            &source,
            &agent::agent_pda(&operator.pubkey()),
            &user.pubkey(),
            300 * ONE,
            &[&user],
        )
        .await
        .unwrap();
    Scene {
        env,
        remit,
        user,
        source,
        payee,
        operator,
    }
}

impl Scene {
    async fn delegated(
        &self,
        operator: &Keypair,
        amount: u64,
    ) -> spl_token_client::token::TokenResult<()> {
        let fee = remit::current_fee(&self.env, &self.remit.token, amount).await?;
        let ix = agent::delegated_transfer(
            &self.source,
            &self.remit.mint,
            &self.payee,
            &operator.pubkey(),
            amount,
            DECIMALS,
            fee,
        );
        self.remit
            .token
            .process_ixs(&[ix], &[operator])
            .await
            .map(|_| ())
    }

    async fn owner_signed(&self, amount: u64) -> spl_token_client::token::TokenResult<()> {
        let fee = remit::current_fee(&self.env, &self.remit.token, amount).await?;
        let ix = agent::owner_transfer(
            &self.source,
            &self.remit.mint,
            &self.payee,
            &self.user.pubkey(),
            amount,
            DECIMALS,
            fee,
        );
        self.remit
            .token
            .process_ixs(&[ix], &[&self.user])
            .await
            .map(|_| ())
    }

    async fn enable_cpi_guard(&self) {
        // CpiGuard lives on the token account, so the account needs room for it.
        self.remit
            .token
            .reallocate(
                &self.source,
                &self.user.pubkey(),
                &[ExtensionType::CpiGuard],
                &[&self.user],
            )
            .await
            .unwrap();
        self.remit
            .token
            .enable_cpi_guard(&self.source, &self.user.pubkey(), &[&self.user])
            .await
            .unwrap();
        let raw = state::read(&self.remit.token, &self.source).await.unwrap();
        let guard = *raw.token_account().get_extension::<CpiGuard>().unwrap();
        assert!(bool::from(guard.lock_cpi));
    }

    async fn delegated_amount(&self) -> u64 {
        let raw = state::read(&self.remit.token, &self.source).await.unwrap();
        raw.token_account().base.delegated_amount
    }
}

#[tokio::test]
async fn operator_moves_funds_through_the_delegated_pda() {
    let s = scene().await;
    s.delegated(&s.operator, 100 * ONE).await.unwrap();
    // 100 RMTD with a 0.5 RMTD fee withheld at the payee.
    assert_eq!(state::amount(&s.remit.token, &s.source).await, 900 * ONE);
    assert_eq!(
        state::amount(&s.remit.token, &s.payee).await,
        100 * ONE - ONE / 2
    );
    assert_eq!(s.delegated_amount().await, 200 * ONE);

    // The allowance is a hard cap.
    assert_token_error(
        s.delegated(&s.operator, 250 * ONE).await,
        TokenError::InsufficientFunds,
    );
}

#[tokio::test]
async fn other_operators_cannot_use_the_delegation() {
    let s = scene().await;

    // Another operator's PDA was never approved.
    let other = Keypair::new();
    assert_token_error(
        s.delegated(&other, 10 * ONE).await,
        TokenError::OwnerMismatch,
    );

    // Nor can it pass the approved PDA: the program re-derives it from the
    // signing operator.
    let fee = remit::current_fee(&s.env, &s.remit.token, 10 * ONE)
        .await
        .unwrap();
    let mut ix = agent::delegated_transfer(
        &s.source,
        &s.remit.mint,
        &s.payee,
        &other.pubkey(),
        10 * ONE,
        DECIMALS,
        fee,
    );
    ix.accounts[3] = AccountMeta::new_readonly(agent::agent_pda(&s.operator.pubkey()), false);
    assert_instruction_error(
        s.remit.token.process_ixs(&[ix], &[&other]).await,
        InstructionError::InvalidSeeds,
    );

    // And the operator must sign.
    let ix = agent::delegated_transfer(
        &s.source,
        &s.remit.mint,
        &s.payee,
        &s.operator.pubkey(),
        10 * ONE,
        DECIMALS,
        fee,
    );
    let unsigned = Instruction {
        accounts: ix
            .accounts
            .iter()
            .map(|a| AccountMeta {
                is_signer: false,
                ..a.clone()
            })
            .collect(),
        ..ix
    };
    assert_instruction_error(
        s.remit
            .token
            .process_ixs(&[unsigned], &Vec::<&Keypair>::new())
            .await,
        InstructionError::MissingRequiredSignature,
    );
}

#[tokio::test]
async fn cpi_guard_blocks_owner_signed_cpi_but_not_the_delegated_path() {
    let s = scene().await;

    // Before CPI Guard both paths work.
    s.delegated(&s.operator, 50 * ONE).await.unwrap();
    s.owner_signed(50 * ONE).await.unwrap();
    assert_eq!(state::amount(&s.remit.token, &s.source).await, 900 * ONE);

    s.enable_cpi_guard().await;

    // The delegated path, same instruction, still works.
    s.delegated(&s.operator, 50 * ONE).await.unwrap();
    assert_eq!(state::amount(&s.remit.token, &s.source).await, 850 * ONE);
    assert_eq!(s.delegated_amount().await, 200 * ONE);

    // The owner's signature forwarded through a program is now refused.
    assert_token_error(
        s.owner_signed(50 * ONE).await,
        TokenError::CpiGuardTransferBlocked,
    );
    assert_eq!(state::amount(&s.remit.token, &s.source).await, 850 * ONE);

    // A direct, top-level owner transfer is unaffected.
    s.remit
        .transfer(&s.source, &s.payee, &s.user.pubkey(), &[&s.user], 50 * ONE)
        .await
        .unwrap();
    assert_eq!(state::amount(&s.remit.token, &s.source).await, 800 * ONE);
}
