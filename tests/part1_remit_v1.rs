//! Part 1: public remittance mint.
//! Transfer fee + metadata pointer (metadata on the mint) + default frozen
//! accounts + mint close authority.

mod common;

use {
    common::assert_token_error,
    remit_token22::{
        remit::{self, plan_mint, RemitV1, Roles, DECIMALS, MAXIMUM_FEE, ONE},
        state, Env,
    },
    solana_keypair::Keypair,
    solana_pubkey::Pubkey,
    solana_signer::Signer,
    spl_token_2022_interface::{
        error::TokenError,
        extension::{
            default_account_state::DefaultAccountState,
            mint_close_authority::MintCloseAuthority,
            transfer_fee::{TransferFeeAmount, TransferFeeConfig},
            BaseStateWithExtensions, ExtensionType,
        },
        instruction::TokenInstruction,
        state::{AccountState, Mint},
    },
    spl_token_metadata_interface::instruction::TokenMetadataInstruction,
};

/// Opens an account for `owner` and passes KYC (thaw).
async fn onboard(remit: &RemitV1, owner: &Keypair) -> Pubkey {
    let account = remit.open_account(&owner.pubkey()).await.unwrap();
    remit.kyc_approve(&account).await.unwrap();
    account
}

async fn withheld_in(remit: &RemitV1, account: &Pubkey) -> u64 {
    let raw = state::read(&remit.token, account).await.unwrap();
    raw.token_account()
        .get_extension::<TransferFeeAmount>()
        .unwrap()
        .withheld_amount
        .into()
}

async fn mint_withheld(remit: &RemitV1) -> u64 {
    let raw = state::read(&remit.token, &remit.mint).await.unwrap();
    raw.mint()
        .get_extension::<TransferFeeConfig>()
        .unwrap()
        .withheld_amount
        .into()
}

#[tokio::test]
async fn creation_inits_every_extension_before_initialize_mint() {
    let env = Env::start().await;
    let rent = env.rent().await;
    let mint = Pubkey::new_unique();
    let plan = plan_mint(&env.payer_pubkey(), &mint, &Roles::generate(), None, &rent).unwrap();

    // Sized from the extension list, not hard-coded.
    assert_eq!(
        plan.space,
        ExtensionType::try_calculate_account_len::<Mint>(&RemitV1::EXTENSIONS).unwrap()
    );

    // Decode the actual instruction list.
    let ixs = &plan.instructions;
    assert_eq!(ixs[0].program_id, solana_system_interface::program::id());
    let steps: Vec<&str> = ixs[1..]
        .iter()
        .map(|ix| match TokenInstruction::unpack(&ix.data) {
            Ok(TokenInstruction::TransferFeeExtension) => "TransferFeeConfig",
            Ok(TokenInstruction::MetadataPointerExtension) => "MetadataPointer",
            Ok(TokenInstruction::DefaultAccountStateExtension) => "DefaultAccountState",
            Ok(TokenInstruction::InitializeMintCloseAuthority { .. }) => "MintCloseAuthority",
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
            "InitializeMint2",
            "TokenMetadata::Initialize",
            "TokenMetadata::UpdateField",
        ]
    );

    // All of it goes out as one transaction, which has to fit in a packet.
    let size = common::wire_size(ixs, &env.payer_pubkey());
    assert!(size <= solana_packet::PACKET_DATA_SIZE, "{size} bytes");
}

#[tokio::test]
async fn mint_is_created_with_all_four_extensions() {
    let env = Env::start().await;
    let remit = RemitV1::create(&env).await.unwrap();
    let raw = state::read(&remit.token, &remit.mint).await.unwrap();
    let mint = raw.mint();

    let types = mint.get_extension_types().unwrap();
    for ext in RemitV1::EXTENSIONS {
        assert!(types.contains(&ext), "missing {ext:?}");
    }
    assert!(types.contains(&ExtensionType::TokenMetadata));

    assert_eq!(mint.base.decimals, DECIMALS);
    assert_eq!(mint.base.supply, 0);
    assert_eq!(
        Option::<Pubkey>::from(mint.base.freeze_authority),
        Some(remit.roles.freeze_authority.pubkey())
    );

    let fee = mint.get_extension::<TransferFeeConfig>().unwrap();
    assert_eq!(
        u16::from(fee.newer_transfer_fee.transfer_fee_basis_points),
        50
    );
    assert_eq!(u64::from(fee.newer_transfer_fee.maximum_fee), MAXIMUM_FEE);

    let default_state = mint.get_extension::<DefaultAccountState>().unwrap();
    assert_eq!(default_state.state, AccountState::Frozen as u8);

    let close = mint.get_extension::<MintCloseAuthority>().unwrap();
    assert_eq!(
        Option::<Pubkey>::from(close.close_authority),
        Some(remit.roles.close_authority.pubkey())
    );
}

#[tokio::test]
async fn metadata_lives_on_the_mint_and_its_rent_was_paid_up_front() {
    let env = Env::start().await;
    let remit = RemitV1::create(&env).await.unwrap();

    let (pointer, metadata) = remit::read_metadata(&remit.token).await.unwrap();
    assert_eq!(
        Option::<Pubkey>::from(pointer.metadata_address),
        Some(remit.mint)
    );
    assert_eq!(
        Option::<Pubkey>::from(pointer.authority),
        Some(remit.roles.metadata_authority.pubkey())
    );
    assert_eq!(metadata.mint, remit.mint);
    assert_eq!(metadata.name, remit::NAME);
    assert_eq!(metadata.symbol, remit::SYMBOL);
    assert_eq!(metadata.uri, remit::URI);
    assert_eq!(
        metadata.additional_metadata,
        vec![("peg".to_string(), "USD".to_string())]
    );

    // The account grew from the fixed extension size to fit the
    // variable-length TokenMetadata, and the lamports funded at creation
    // cover exactly that final size.
    let raw = state::read(&remit.token, &remit.mint).await.unwrap();
    let fixed = ExtensionType::try_calculate_account_len::<Mint>(&RemitV1::EXTENSIONS).unwrap();
    assert_eq!(
        raw.data.len(),
        fixed + remit::metadata_entry_len(&metadata).unwrap()
    );
    let rent = env.rent().await;
    assert_eq!(raw.lamports, rent.minimum_balance(raw.data.len()));
}

#[tokio::test]
async fn kyc_thaws_one_account_and_leaves_the_mint_default_frozen() {
    let env = Env::start().await;
    let remit = RemitV1::create(&env).await.unwrap();
    let user = Keypair::new();

    let account = remit.open_account(&user.pubkey()).await.unwrap();
    let raw = state::read(&remit.token, &account).await.unwrap();
    assert_eq!(raw.token_account().base.state, AccountState::Frozen);

    // Nothing can land in a frozen account, not even a mint.
    assert_token_error(
        remit.mint_to(&account, 100 * ONE).await,
        TokenError::AccountFrozen,
    );

    // Only the freeze authority can thaw.
    let stranger = Keypair::new();
    assert_token_error(
        remit
            .token
            .thaw(&account, &stranger.pubkey(), &[&stranger])
            .await,
        TokenError::OwnerMismatch,
    );

    remit.kyc_approve(&account).await.unwrap();
    let raw = state::read(&remit.token, &account).await.unwrap();
    assert_eq!(raw.token_account().base.state, AccountState::Initialized);
    remit.mint_to(&account, 100 * ONE).await.unwrap();
    assert_eq!(state::amount(&remit.token, &account).await, 100 * ONE);

    // The thaw was per account: the mint-level default is still Frozen...
    let raw = state::read(&remit.token, &remit.mint).await.unwrap();
    let default_state = raw
        .mint()
        .get_extension::<DefaultAccountState>()
        .unwrap()
        .state;
    assert_eq!(default_state, AccountState::Frozen as u8);
    // ...so the next account opened is frozen again.
    let next = remit.open_account(&Keypair::new().pubkey()).await.unwrap();
    let raw = state::read(&remit.token, &next).await.unwrap();
    assert_eq!(raw.token_account().base.state, AccountState::Frozen);
}

#[tokio::test]
async fn compliance_can_freeze_an_account_again() {
    let env = Env::start().await;
    let remit = RemitV1::create(&env).await.unwrap();
    let (sender, receiver) = (Keypair::new(), Keypair::new());
    let from = onboard(&remit, &sender).await;
    let to = onboard(&remit, &receiver).await;
    remit.mint_to(&from, 50 * ONE).await.unwrap();

    let freeze = &remit.roles.freeze_authority;
    remit
        .token
        .freeze(&from, &freeze.pubkey(), &[freeze])
        .await
        .unwrap();
    assert_token_error(
        remit
            .transfer(&from, &to, &sender.pubkey(), &[&sender], 10 * ONE)
            .await,
        TokenError::AccountFrozen,
    );
}

#[tokio::test]
async fn transfer_uses_transfer_checked_with_fee_at_the_epoch_rate() {
    let env = Env::start().await;
    let remit = RemitV1::create(&env).await.unwrap();
    let (sender, receiver) = (Keypair::new(), Keypair::new());
    let from = onboard(&remit, &sender).await;
    let to = onboard(&remit, &receiver).await;
    remit.mint_to(&from, 5_000 * ONE).await.unwrap();

    // 200 RMTD at 0.50% -> 1 RMTD fee, withheld in the receiver's account.
    let fee = remit
        .transfer(&from, &to, &sender.pubkey(), &[&sender], 200 * ONE)
        .await
        .unwrap();
    assert_eq!(fee, ONE);
    assert_eq!(state::amount(&remit.token, &to).await, 199 * ONE);
    assert_eq!(withheld_in(&remit, &to).await, ONE);

    // TransferCheckedWithFee asserts the fee: a wrong one is rejected.
    assert_token_error(
        remit::transfer_with_fee_unchecked(
            &remit.token,
            &from,
            &to,
            &sender.pubkey(),
            &[&sender],
            200 * ONE,
            ONE - 1,
        )
        .await,
        TokenError::FeeMismatch,
    );

    // 2,000 RMTD would be a 10 RMTD fee, but it is capped at 5.
    let fee = remit
        .transfer(&from, &to, &sender.pubkey(), &[&sender], 2_000 * ONE)
        .await
        .unwrap();
    assert_eq!(fee, MAXIMUM_FEE);
    assert_eq!(withheld_in(&remit, &to).await, ONE + MAXIMUM_FEE);
}

#[tokio::test]
async fn fee_changes_apply_from_their_epoch_because_nothing_is_cached() {
    let env = Env::start().await;
    let remit = RemitV1::create(&env).await.unwrap();
    let (sender, receiver) = (Keypair::new(), Keypair::new());
    let from = onboard(&remit, &sender).await;
    let to = onboard(&remit, &receiver).await;
    remit.mint_to(&from, 1_000 * ONE).await.unwrap();

    // Raise the fee to 1.00%, max 10. Token-2022 schedules it two epochs out.
    let authority = &remit.roles.fee_config_authority;
    remit
        .token
        .set_transfer_fee(&authority.pubkey(), 100, 10 * ONE, &[authority])
        .await
        .unwrap();
    let epoch = env.clock().await.epoch;
    let raw = state::read(&remit.token, &remit.mint).await.unwrap();
    let config = *raw.mint().get_extension::<TransferFeeConfig>().unwrap();
    assert_eq!(u64::from(config.newer_transfer_fee.epoch), epoch + 2);

    // Same epoch: still 0.50%.
    let fee = remit
        .transfer(&from, &to, &sender.pubkey(), &[&sender], 100 * ONE)
        .await
        .unwrap();
    assert_eq!(fee, ONE / 2);

    // Two epochs later the Clock says the new rate is live.
    env.warp_to_epoch(epoch + 2).await;
    assert_eq!(env.clock().await.epoch, epoch + 2);
    let fee = remit
        .transfer(&from, &to, &sender.pubkey(), &[&sender], 100 * ONE)
        .await
        .unwrap();
    assert_eq!(fee, ONE);
    assert_eq!(withheld_in(&remit, &to).await, ONE / 2 + ONE);
}

#[tokio::test]
async fn fee_desk_withdraws_withheld_fees_from_accounts() {
    let env = Env::start().await;
    let remit = RemitV1::create(&env).await.unwrap();
    let (sender, receiver, fee_desk_owner) = (Keypair::new(), Keypair::new(), Keypair::new());
    let from = onboard(&remit, &sender).await;
    let to = onboard(&remit, &receiver).await;
    let fee_desk = onboard(&remit, &fee_desk_owner).await;
    remit.mint_to(&from, 1_000 * ONE).await.unwrap();
    let fee = remit
        .transfer(&from, &to, &sender.pubkey(), &[&sender], 600 * ONE)
        .await
        .unwrap();
    assert_eq!(fee, 3 * ONE);

    // Only the withdraw-withheld authority can pull the fees out.
    let stranger = Keypair::new();
    assert_token_error(
        remit
            .token
            .withdraw_withheld_tokens_from_accounts(
                &fee_desk,
                &stranger.pubkey(),
                &[&to],
                &[&stranger],
            )
            .await,
        TokenError::OwnerMismatch,
    );

    let authority = &remit.roles.withdraw_withheld_authority;
    remit
        .token
        .withdraw_withheld_tokens_from_accounts(
            &fee_desk,
            &authority.pubkey(),
            &[&to],
            &[authority],
        )
        .await
        .unwrap();
    assert_eq!(withheld_in(&remit, &to).await, 0);
    assert_eq!(state::amount(&remit.token, &fee_desk).await, fee);
    // Fees are moved, not burned: supply is unchanged.
    assert_eq!(state::supply(&remit.token).await, 1_000 * ONE);
}

#[tokio::test]
async fn fees_can_be_harvested_to_the_mint_then_withdrawn() {
    let env = Env::start().await;
    let remit = RemitV1::create(&env).await.unwrap();
    let (sender, receiver, fee_desk_owner) = (Keypair::new(), Keypair::new(), Keypair::new());
    let from = onboard(&remit, &sender).await;
    let to = onboard(&remit, &receiver).await;
    let fee_desk = onboard(&remit, &fee_desk_owner).await;
    remit.mint_to(&from, 1_000 * ONE).await.unwrap();
    let fee = remit
        .transfer(&from, &to, &sender.pubkey(), &[&sender], 400 * ONE)
        .await
        .unwrap();

    // Harvesting is permissionless: it just moves withheld fees into the mint.
    remit
        .token
        .harvest_withheld_tokens_to_mint(&[&to])
        .await
        .unwrap();
    assert_eq!(withheld_in(&remit, &to).await, 0);
    assert_eq!(mint_withheld(&remit).await, fee);

    let authority = &remit.roles.withdraw_withheld_authority;
    remit
        .token
        .withdraw_withheld_tokens_from_mint(&fee_desk, &authority.pubkey(), &[authority])
        .await
        .unwrap();
    assert_eq!(state::amount(&remit.token, &fee_desk).await, fee);
    assert_eq!(mint_withheld(&remit).await, 0);
}

#[tokio::test]
async fn mint_can_only_be_closed_by_its_close_authority_at_zero_supply() {
    let env = Env::start().await;
    let remit = RemitV1::create(&env).await.unwrap();
    let (sender, receiver, fee_desk_owner) = (Keypair::new(), Keypair::new(), Keypair::new());
    let from = onboard(&remit, &sender).await;
    let to = onboard(&remit, &receiver).await;
    let fee_desk = onboard(&remit, &fee_desk_owner).await;
    remit.mint_to(&from, 100 * ONE).await.unwrap();
    let fee = remit
        .transfer(&from, &to, &sender.pubkey(), &[&sender], 100 * ONE)
        .await
        .unwrap();

    let close = &remit.roles.close_authority;
    let rent_to = env.payer_pubkey();

    // Supply > 0: refused, even for the right key.
    assert_token_error(
        remit
            .token
            .close_account(&remit.mint, &rent_to, &close.pubkey(), &[close])
            .await,
        TokenError::MintHasSupply,
    );

    // Wind down: collect the withheld fee, then every holder burns.
    let authority = &remit.roles.withdraw_withheld_authority;
    remit
        .token
        .withdraw_withheld_tokens_from_accounts(
            &fee_desk,
            &authority.pubkey(),
            &[&to],
            &[authority],
        )
        .await
        .unwrap();
    remit
        .token
        .burn(&to, &receiver.pubkey(), 100 * ONE - fee, &[&receiver])
        .await
        .unwrap();
    remit
        .token
        .burn(&fee_desk, &fee_desk_owner.pubkey(), fee, &[&fee_desk_owner])
        .await
        .unwrap();
    assert_eq!(state::supply(&remit.token).await, 0);

    // Wrong key is still refused.
    let stranger = Keypair::new();
    assert_token_error(
        remit
            .token
            .close_account(&remit.mint, &rent_to, &stranger.pubkey(), &[&stranger])
            .await,
        TokenError::OwnerMismatch,
    );

    remit
        .token
        .close_account(&remit.mint, &rent_to, &close.pubkey(), &[close])
        .await
        .unwrap();
    assert!(
        env.try_read(&remit.mint).await.is_none(),
        "mint account should be closed"
    );
}
