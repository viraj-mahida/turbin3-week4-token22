//! Part 2: v1 + permanent delegate + confidential transfers.
//! Full confidential lifecycle: configure -> deposit -> apply pending ->
//! confidential transfer (with confidential fee) -> withdraw.

mod common;

use {
    common::{
        assert_instruction_error, assert_token_error, fund_confidential, onboard_confidential,
        transfer_keys,
    },
    remit_token22::{
        confidential::{self, Balances},
        remit::{
            self, plan_mint, ConfidentialRoles, RemitV1, RemitV2, Roles, DECIMALS, MAXIMUM_FEE, ONE,
        },
        state, Env,
    },
    solana_instruction_error::InstructionError,
    solana_keypair::Keypair,
    solana_signer::Signer,
    spl_associated_token_account_interface::instruction::create_associated_token_account,
    spl_token_2022_interface::{
        error::TokenError,
        extension::{
            confidential_transfer::ConfidentialTransferMint,
            default_account_state::DefaultAccountState, metadata_pointer::MetadataPointer,
            mint_close_authority::MintCloseAuthority, permanent_delegate::PermanentDelegate,
            transfer_fee::TransferFeeConfig, BaseStateWithExtensions, ExtensionType,
        },
        instruction::TokenInstruction,
        state::{AccountState, Mint},
    },
    spl_token_client::{
        token::TokenError as ClientError, zk_proofs::confidential_transfer::WithdrawAccountInfo,
    },
    spl_token_metadata_interface::instruction::TokenMetadataInstruction,
};

const ONE_SOL: u64 = 1_000_000_000;

#[tokio::test]
async fn v2_creation_carries_v1_forward_and_inits_everything_before_the_mint() {
    let env = Env::start().await;
    let rent = env.rent().await;
    let mint = solana_pubkey::Pubkey::new_unique();
    let confidential_roles = ConfidentialRoles::generate();
    let plan = plan_mint(
        &env.payer_pubkey(),
        &mint,
        &Roles::generate(),
        Some((&confidential_roles, false)),
        &rent,
    )
    .unwrap();

    // Every v1 extension is still there, plus the two new ones.
    for ext in RemitV1::EXTENSIONS {
        assert!(RemitV2::EXTENSIONS.contains(&ext), "v2 dropped {ext:?}");
    }
    assert_eq!(
        plan.space,
        ExtensionType::try_calculate_account_len::<Mint>(&RemitV2::EXTENSIONS).unwrap()
    );

    let ixs = &plan.instructions;
    assert_eq!(ixs[0].program_id, solana_system_interface::program::id());
    let steps: Vec<&str> = ixs[1..]
        .iter()
        .map(|ix| match TokenInstruction::unpack(&ix.data) {
            Ok(TokenInstruction::TransferFeeExtension) => "TransferFeeConfig",
            Ok(TokenInstruction::MetadataPointerExtension) => "MetadataPointer",
            Ok(TokenInstruction::DefaultAccountStateExtension) => "DefaultAccountState",
            Ok(TokenInstruction::InitializeMintCloseAuthority { .. }) => "MintCloseAuthority",
            Ok(TokenInstruction::InitializePermanentDelegate { .. }) => "PermanentDelegate",
            Ok(TokenInstruction::ConfidentialTransferExtension) => "ConfidentialTransferMint",
            Ok(TokenInstruction::ConfidentialTransferFeeExtension) => {
                "ConfidentialTransferFeeConfig"
            }
            Ok(TokenInstruction::InitializeMint2 { .. }) => "InitializeMint2",
            _ => match TokenMetadataInstruction::unpack(&ix.data) {
                Ok(TokenMetadataInstruction::Initialize(_)) => "TokenMetadata::Initialize",
                Ok(TokenMetadataInstruction::UpdateField(_)) => "TokenMetadata::UpdateField",
                _ => "unknown",
            },
        })
        .collect();
    assert_eq!(
        steps,
        [
            "TransferFeeConfig",
            "MetadataPointer",
            "DefaultAccountState",
            "MintCloseAuthority",
            "PermanentDelegate",
            "ConfidentialTransferMint",
            "ConfidentialTransferFeeConfig",
            "InitializeMint2",
            "TokenMetadata::Initialize",
            "TokenMetadata::UpdateField",
        ]
    );
    let size = common::wire_size(ixs, &env.payer_pubkey());
    assert!(size <= solana_packet::PACKET_DATA_SIZE, "{size} bytes");
}

#[tokio::test]
async fn v2_mint_adds_permanent_delegate_and_manual_confidential_approval() {
    let env = Env::start().await;
    let remit = RemitV2::create(&env).await.unwrap();
    let raw = state::read(&remit.token, &remit.mint).await.unwrap();
    let mint = raw.mint();

    let types = mint.get_extension_types().unwrap();
    for ext in RemitV2::EXTENSIONS {
        assert!(types.contains(&ext), "missing {ext:?}");
    }
    assert!(types.contains(&ExtensionType::TokenMetadata));

    // v1 settings carried forward unchanged.
    let fee = mint.get_extension::<TransferFeeConfig>().unwrap();
    assert_eq!(
        u16::from(fee.newer_transfer_fee.transfer_fee_basis_points),
        50
    );
    assert_eq!(u64::from(fee.newer_transfer_fee.maximum_fee), MAXIMUM_FEE);
    let default_state = mint.get_extension::<DefaultAccountState>().unwrap();
    assert_eq!(default_state.state, AccountState::Frozen as u8);
    let pointer = mint.get_extension::<MetadataPointer>().unwrap();
    assert_eq!(
        Option::<solana_pubkey::Pubkey>::from(pointer.metadata_address),
        Some(remit.mint)
    );
    assert!(mint.get_extension::<MintCloseAuthority>().is_ok());

    let delegate = mint.get_extension::<PermanentDelegate>().unwrap();
    assert_eq!(
        Option::<solana_pubkey::Pubkey>::from(delegate.delegate),
        Some(remit.confidential.permanent_delegate.pubkey())
    );

    // approve_policy = manual
    let ct = mint.get_extension::<ConfidentialTransferMint>().unwrap();
    assert_eq!(
        Option::<solana_pubkey::Pubkey>::from(ct.authority),
        Some(remit.confidential.confidential_authority.pubkey())
    );
    assert!(!bool::from(ct.auto_approve_new_accounts));
    let auditor: Option<solana_zk_sdk_pod::encryption::elgamal::PodElGamalPubkey> =
        ct.auditor_elgamal_pubkey.into();
    assert_eq!(auditor, Some((*remit.confidential.auditor.pubkey()).into()));

    let (_, metadata) = remit::read_metadata(&remit.token).await.unwrap();
    assert_eq!(metadata.symbol, remit::SYMBOL);
    let rent = env.rent().await;
    assert_eq!(raw.lamports, rent.minimum_balance(raw.data.len()));
}

#[tokio::test]
async fn anyone_can_create_the_ata_but_only_the_owner_can_configure_it() {
    let env = Env::start().await;
    let remit = RemitV2::create(&env).await.unwrap();
    let (owner, third_party) = (Keypair::new(), Keypair::new());

    // The third party funds and creates the owner's ATA.
    remit
        .token
        .process_ixs(
            &[solana_system_interface::instruction::transfer(
                &env.payer_pubkey(),
                &third_party.pubkey(),
                ONE_SOL,
            )],
            &Vec::<&Keypair>::new(),
        )
        .await
        .unwrap();
    remit
        .token
        .process_ixs(
            &[create_associated_token_account(
                &third_party.pubkey(),
                &owner.pubkey(),
                &remit.mint,
                &spl_token_2022_interface::id(),
            )],
            &[&third_party],
        )
        .await
        .unwrap();
    let account = remit.token.get_associated_token_address(&owner.pubkey());
    let raw = state::read(&remit.token, &account).await.unwrap();
    assert_eq!(raw.token_account().base.owner, owner.pubkey());
    remit.kyc_approve(&account).await.unwrap();

    // It cannot add the confidential extension space...
    let extensions = [
        ExtensionType::ConfidentialTransferAccount,
        ExtensionType::ConfidentialTransferFeeAmount,
    ];
    assert_token_error(
        remit
            .token
            .reallocate(
                &account,
                &third_party.pubkey(),
                &extensions,
                &[&third_party],
            )
            .await,
        TokenError::OwnerMismatch,
    );
    // ...and once the space exists, it still cannot ConfigureAccount.
    remit
        .token
        .reallocate(&account, &owner.pubkey(), &extensions, &[&owner])
        .await
        .unwrap();
    let (elgamal, aes) = confidential::derive_keys(&third_party);
    assert_token_error(
        remit
            .token
            .confidential_transfer_configure_token_account(
                &account,
                &third_party.pubkey(),
                None,
                None,
                &elgamal,
                &aes,
                &[&third_party],
            )
            .await,
        TokenError::OwnerMismatch,
    );
    assert!(confidential::raw_state(&remit.token, &account)
        .await
        .is_err());

    // The owner can.
    let configured = confidential::configure(&remit.token, &account, &owner)
        .await
        .unwrap();
    let ct = confidential::raw_state(&remit.token, &account)
        .await
        .unwrap();
    assert_eq!(ct.elgamal_pubkey, (*configured.elgamal.pubkey()).into());
    assert!(!bool::from(ct.approved));
}

#[tokio::test]
async fn pending_balance_must_be_applied_before_withdraw() {
    let env = Env::start().await;
    let remit = RemitV2::create(&env).await.unwrap();
    let user = Keypair::new();
    let account = onboard_confidential(&remit, &user).await;
    // Below 2^16 base units so the whole amount sits in pending_balance_lo.
    let amount = 50_000;
    remit.mint_to(&account.address, amount).await.unwrap();
    confidential::deposit(&remit.token, &account, &user, amount, DECIMALS)
        .await
        .unwrap();

    // Honest client: available is 0, so it cannot even build a proof.
    let err = confidential::withdraw(
        &remit.token,
        &env.payer_pubkey(),
        &account,
        &user.pubkey(),
        &[&user],
        amount,
        DECIMALS,
    )
    .await
    .unwrap_err();
    assert!(matches!(err, ClientError::ProofGeneration), "{err:?}");

    // Proofs built as if the pending ciphertext were available verify in the
    // ZK program, but Token-2022 checks them against the real available
    // balance and rejects the withdraw.
    let ct = confidential::raw_state(&remit.token, &account.address)
        .await
        .unwrap();
    let pretend = WithdrawAccountInfo {
        available_balance: ct.pending_balance_lo,
        decryptable_available_balance: account.aes.encrypt(amount).into(),
    };
    assert_token_error(
        confidential::withdraw_against(
            &remit.token,
            &env.payer_pubkey(),
            &account,
            &user.pubkey(),
            &[&user],
            amount,
            DECIMALS,
            pretend,
        )
        .await,
        TokenError::ConfidentialTransferBalanceMismatch,
    );

    // Apply pending, then the same withdraw goes through.
    confidential::apply_pending(&remit.token, &account, &user)
        .await
        .unwrap();
    confidential::withdraw(
        &remit.token,
        &env.payer_pubkey(),
        &account,
        &user.pubkey(),
        &[&user],
        amount,
        DECIMALS,
    )
    .await
    .unwrap();
    let b = confidential::balances(&remit.token, &account)
        .await
        .unwrap();
    assert_eq!(
        b,
        Balances {
            public: amount,
            pending: 0,
            available: 0
        }
    );
}

#[tokio::test]
async fn accounts_need_kyc_thaw_and_confidential_approval() {
    let env = Env::start().await;
    let remit = RemitV2::create(&env).await.unwrap();
    let user = Keypair::new();

    let account = remit.open_account(&user.pubkey()).await.unwrap();
    remit.kyc_approve(&account).await.unwrap();
    let account = confidential::configure(&remit.token, &account, &user)
        .await
        .unwrap();
    remit.mint_to(&account.address, 10 * ONE).await.unwrap();

    // Configured but not approved: no confidential deposits.
    assert_token_error(
        confidential::deposit(&remit.token, &account, &user, 10 * ONE, DECIMALS).await,
        TokenError::ConfidentialTransferAccountNotApproved,
    );

    // Only the confidential transfer authority can approve.
    let stranger = Keypair::new();
    assert_instruction_error(
        remit
            .token
            .confidential_transfer_approve_account(
                &account.address,
                &stranger.pubkey(),
                &[&stranger],
            )
            .await,
        InstructionError::MissingRequiredSignature,
    );

    remit.approve_confidential(&account.address).await.unwrap();
    confidential::deposit(&remit.token, &account, &user, 10 * ONE, DECIMALS)
        .await
        .unwrap();
}

#[tokio::test]
async fn full_confidential_lifecycle_with_confidential_fee() {
    let env = Env::start().await;
    let remit = RemitV2::create(&env).await.unwrap();
    let payer = env.payer.pubkey();
    let (sender, receiver, fee_desk_owner) = (Keypair::new(), Keypair::new(), Keypair::new());

    // 1. configure (and approve)
    let from = onboard_confidential(&remit, &sender).await;
    let to = onboard_confidential(&remit, &receiver).await;
    let fee_desk = onboard_confidential(&remit, &fee_desk_owner).await;

    remit.mint_to(&from.address, 1_000 * ONE).await.unwrap();
    let b = confidential::balances(&remit.token, &from).await.unwrap();
    assert_eq!(
        b,
        Balances {
            public: 1_000 * ONE,
            pending: 0,
            available: 0
        }
    );

    // 2. deposit: public -> pending
    confidential::deposit(&remit.token, &from, &sender, 400 * ONE, DECIMALS)
        .await
        .unwrap();
    let b = confidential::balances(&remit.token, &from).await.unwrap();
    assert_eq!(
        b,
        Balances {
            public: 600 * ONE,
            pending: 400 * ONE,
            available: 0
        }
    );

    // 3. apply pending: pending -> available
    confidential::apply_pending(&remit.token, &from, &sender)
        .await
        .unwrap();
    let b = confidential::balances(&remit.token, &from).await.unwrap();
    assert_eq!(
        b,
        Balances {
            public: 600 * ONE,
            pending: 0,
            available: 400 * ONE
        }
    );

    // 4. confidential transfer of 100 RMTD; 0.5 RMTD fee withheld, encrypted
    let amount = 100 * ONE;
    let fee = ONE / 2;
    let keys = transfer_keys(&remit, to.elgamal.pubkey()).await;
    let audit = confidential::transfer_with_fee(
        &remit.token,
        &payer,
        &from,
        &to.address,
        &sender.pubkey(),
        &[&sender],
        amount,
        &keys,
    )
    .await
    .unwrap();

    // The auditor can read the amount from the instruction's ciphertexts.
    assert_eq!(audit.decrypt(&remit.confidential.auditor), Some(amount));

    let b = confidential::balances(&remit.token, &from).await.unwrap();
    assert_eq!(
        b,
        Balances {
            public: 600 * ONE,
            pending: 0,
            available: 300 * ONE
        }
    );
    let b = confidential::balances(&remit.token, &to).await.unwrap();
    assert_eq!(
        b,
        Balances {
            public: 0,
            pending: amount - fee,
            available: 0
        }
    );

    // The public amount field of both accounts never showed the 100.
    assert_eq!(state::amount(&remit.token, &to.address).await, 0);

    // The withheld fee is encrypted to the fee desk's ElGamal key.
    let withheld = confidential::withheld_confidential_fee(
        &remit.token,
        &to.address,
        &remit.confidential.withdraw_withheld_elgamal,
    )
    .await
    .unwrap();
    assert_eq!(withheld, fee);

    // Receiver applies the credit.
    confidential::apply_pending(&remit.token, &to, &receiver)
        .await
        .unwrap();
    let b = confidential::balances(&remit.token, &to).await.unwrap();
    assert_eq!(
        b,
        Balances {
            public: 0,
            pending: 0,
            available: amount - fee
        }
    );

    // 5. withdraw: available -> public, e.g. to cash out
    confidential::withdraw(
        &remit.token,
        &payer,
        &to,
        &receiver.pubkey(),
        &[&receiver],
        50 * ONE,
        DECIMALS,
    )
    .await
    .unwrap();
    let b = confidential::balances(&remit.token, &to).await.unwrap();
    assert_eq!(
        b,
        Balances {
            public: 50 * ONE,
            pending: 0,
            available: amount - fee - 50 * ONE
        }
    );

    // Fee desk pulls the confidential fee into its own confidential balance.
    confidential::withdraw_withheld_confidential_fees(
        &remit.token,
        &fee_desk,
        &remit.roles.withdraw_withheld_authority,
        &remit.confidential.withdraw_withheld_elgamal,
        &to.address,
        fee,
    )
    .await
    .unwrap();
    let b = confidential::balances(&remit.token, &fee_desk)
        .await
        .unwrap();
    assert_eq!(
        b,
        Balances {
            public: 0,
            pending: 0,
            available: fee
        }
    );
    let withheld = confidential::withheld_confidential_fee(
        &remit.token,
        &to.address,
        &remit.confidential.withdraw_withheld_elgamal,
    )
    .await
    .unwrap();
    assert_eq!(withheld, 0);

    // Confidential moves never change supply.
    assert_eq!(state::supply(&remit.token).await, 1_000 * ONE);
}

#[tokio::test]
async fn permanent_delegate_can_seize_and_burn_a_public_balance() {
    let env = Env::start().await;
    let remit = RemitV2::create(&env).await.unwrap();
    let (sanctioned, compliance_owner) = (Keypair::new(), Keypair::new());
    let user = remit.open_account(&sanctioned.pubkey()).await.unwrap();
    remit.kyc_approve(&user).await.unwrap();
    let seized = remit
        .open_account(&compliance_owner.pubkey())
        .await
        .unwrap();
    remit.kyc_approve(&seized).await.unwrap();
    remit.mint_to(&user, 300 * ONE).await.unwrap();

    let delegate = &remit.confidential.permanent_delegate;

    // Transfer out without the owner's signature (fee still applies).
    remit
        .transfer(&user, &seized, &delegate.pubkey(), &[delegate], 100 * ONE)
        .await
        .unwrap();
    let fee = ONE / 2;
    assert_eq!(state::amount(&remit.token, &seized).await, 100 * ONE - fee);

    // Burn the rest outright.
    remit
        .token
        .burn(&user, &delegate.pubkey(), 200 * ONE, &[delegate])
        .await
        .unwrap();
    assert_eq!(state::amount(&remit.token, &user).await, 0);
    assert_eq!(state::supply(&remit.token).await, 100 * ONE);

    // Any other key is still just a stranger.
    let stranger = Keypair::new();
    remit.mint_to(&user, 10 * ONE).await.unwrap();
    assert_token_error(
        remit
            .token
            .burn(&user, &stranger.pubkey(), 10 * ONE, &[&stranger])
            .await,
        TokenError::OwnerMismatch,
    );
}

#[tokio::test]
async fn permanent_delegate_cannot_act_on_a_frozen_account() {
    let env = Env::start().await;
    let remit = RemitV2::create(&env).await.unwrap();
    let (sanctioned, compliance_owner) = (Keypair::new(), Keypair::new());
    let user = remit.open_account(&sanctioned.pubkey()).await.unwrap();
    remit.kyc_approve(&user).await.unwrap();
    let seized = remit
        .open_account(&compliance_owner.pubkey())
        .await
        .unwrap();
    remit.kyc_approve(&seized).await.unwrap();
    remit.mint_to(&user, 50 * ONE).await.unwrap();

    let freeze = &remit.roles.freeze_authority;
    remit
        .token
        .freeze(&user, &freeze.pubkey(), &[freeze])
        .await
        .unwrap();

    let delegate = &remit.confidential.permanent_delegate;
    assert_token_error(
        remit
            .token
            .burn(&user, &delegate.pubkey(), 50 * ONE, &[delegate])
            .await,
        TokenError::AccountFrozen,
    );
    assert_token_error(
        remit
            .transfer(&user, &seized, &delegate.pubkey(), &[delegate], 50 * ONE)
            .await,
        TokenError::AccountFrozen,
    );

    // Thaw, seize, and re-freeze (all could go in one transaction).
    remit.kyc_approve(&user).await.unwrap();
    remit
        .token
        .burn(&user, &delegate.pubkey(), 50 * ONE, &[delegate])
        .await
        .unwrap();
    remit
        .token
        .freeze(&user, &freeze.pubkey(), &[freeze])
        .await
        .unwrap();
    assert_eq!(state::amount(&remit.token, &user).await, 0);
}

#[tokio::test]
async fn funded_confidential_account_round_trip() {
    // Same lifecycle via the helper, checking a self-contained deposit ->
    // withdraw round trip leaves the account where it started.
    let env = Env::start().await;
    let remit = RemitV2::create(&env).await.unwrap();
    let user = Keypair::new();
    let account = onboard_confidential(&remit, &user).await;
    fund_confidential(&remit, &account, &user, 75 * ONE).await;
    confidential::withdraw(
        &remit.token,
        &env.payer.pubkey(),
        &account,
        &user.pubkey(),
        &[&user],
        75 * ONE,
        DECIMALS,
    )
    .await
    .unwrap();
    let b = confidential::balances(&remit.token, &account)
        .await
        .unwrap();
    assert_eq!(
        b,
        Balances {
            public: 75 * ONE,
            pending: 0,
            available: 0
        }
    );
}
