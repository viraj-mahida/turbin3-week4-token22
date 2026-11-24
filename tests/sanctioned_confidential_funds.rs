//! What the issuer can and cannot do once a sanctioned user has moved funds
//! into a confidential balance. These tests back TECHNICAL_NOTES.md.

mod common;

use {
    common::{
        assert_instruction_error, assert_token_error, fund_confidential, onboard_confidential,
        transfer_keys,
    },
    remit_token22::{
        confidential::{self, Balances},
        remit::{RemitV2, DECIMALS, ONE},
        state, Env,
    },
    solana_instruction_error::InstructionError,
    solana_keypair::Keypair,
    solana_signer::Signer,
    solana_zk_sdk::encryption::elgamal::ElGamalCiphertext,
    spl_token_2022_interface::error::TokenError,
};

struct Scene {
    env: Env,
    remit: RemitV2,
    sanctioned: Keypair,
    account: confidential::ConfidentialAccount,
    seized: solana_pubkey::Pubkey,
}

/// A user with 500 RMTD, all of it moved into the available confidential
/// balance, plus a compliance account to seize into.
async fn scene() -> Scene {
    let env = Env::start().await;
    let remit = RemitV2::create(&env).await.unwrap();
    let sanctioned = Keypair::new();
    let account = onboard_confidential(&remit, &sanctioned).await;
    fund_confidential(&remit, &account, &sanctioned, 500 * ONE).await;
    let compliance = Keypair::new();
    let seized = remit.open_account(&compliance.pubkey()).await.unwrap();
    remit.kyc_approve(&seized).await.unwrap();
    Scene {
        env,
        remit,
        sanctioned,
        account,
        seized,
    }
}

#[tokio::test]
async fn delegate_cannot_burn_or_transfer_a_confidential_balance() {
    let Scene {
        remit,
        account,
        seized,
        ..
    } = scene().await;
    let delegate = &remit.confidential.permanent_delegate;

    let b = confidential::balances(&remit.token, &account)
        .await
        .unwrap();
    assert_eq!(
        b,
        Balances {
            public: 0,
            pending: 0,
            available: 500 * ONE
        }
    );

    // Burn and transfer only look at the public `amount`, which is 0.
    assert_token_error(
        remit
            .token
            .burn(&account.address, &delegate.pubkey(), ONE, &[delegate])
            .await,
        TokenError::InsufficientFunds,
    );
    assert_token_error(
        remit
            .transfer(
                &account.address,
                &seized,
                &delegate.pubkey(),
                &[delegate],
                ONE,
            )
            .await,
        TokenError::InsufficientFunds,
    );

    // Nothing moved; the tokens still count in supply.
    let b = confidential::balances(&remit.token, &account)
        .await
        .unwrap();
    assert_eq!(b.available, 500 * ONE);
    assert_eq!(state::supply(&remit.token).await, 500 * ONE);
}

#[tokio::test]
async fn delegate_cannot_touch_a_pending_balance_either() {
    let env = Env::start().await;
    let remit = RemitV2::create(&env).await.unwrap();
    let user = Keypair::new();
    let account = onboard_confidential(&remit, &user).await;
    remit.mint_to(&account.address, 80 * ONE).await.unwrap();
    confidential::deposit(&remit.token, &account, &user, 80 * ONE, DECIMALS)
        .await
        .unwrap();

    let delegate = &remit.confidential.permanent_delegate;
    assert_token_error(
        remit
            .token
            .burn(&account.address, &delegate.pubkey(), ONE, &[delegate])
            .await,
        TokenError::InsufficientFunds,
    );
    // Applying the pending balance needs the owner as signer.
    assert_token_error(
        confidential::apply_pending(&remit.token, &account, delegate).await,
        TokenError::OwnerMismatch,
    );
    let b = confidential::balances(&remit.token, &account)
        .await
        .unwrap();
    assert_eq!(
        b,
        Balances {
            public: 0,
            pending: 80 * ONE,
            available: 0
        }
    );
}

#[tokio::test]
async fn delegate_cannot_withdraw_or_transfer_confidentially_even_with_valid_proofs() {
    let Scene {
        env,
        remit,
        account,
        seized,
        ..
    } = scene().await;
    let payer = env.payer.pubkey();
    let delegate = &remit.confidential.permanent_delegate;

    // Worst case for the user: the proofs are built with the user's own keys
    // (as if the delegate had them). The program still checks the signer
    // against the account owner, and the permanent delegate is not accepted.
    assert_token_error(
        confidential::withdraw(
            &remit.token,
            &payer,
            &account,
            &delegate.pubkey(),
            &[delegate],
            10 * ONE,
            DECIMALS,
        )
        .await,
        TokenError::OwnerMismatch,
    );

    let compliance = Keypair::new();
    let target = onboard_confidential(&remit, &compliance).await;
    let keys = transfer_keys(&remit, target.elgamal.pubkey()).await;
    assert_token_error(
        confidential::transfer_with_fee(
            &remit.token,
            &payer,
            &account,
            &target.address,
            &delegate.pubkey(),
            &[delegate],
            10 * ONE,
            &keys,
        )
        .await
        .map(|_| ()),
        TokenError::OwnerMismatch,
    );

    let b = confidential::balances(&remit.token, &account)
        .await
        .unwrap();
    assert_eq!(b.available, 500 * ONE);
    let _ = seized;
}

#[tokio::test]
async fn freeze_locks_a_confidential_balance_in_place_without_seizing_it() {
    let Scene {
        env,
        remit,
        sanctioned,
        account,
        ..
    } = scene().await;
    let payer = env.payer.pubkey();
    let freeze = &remit.roles.freeze_authority;
    let delegate = &remit.confidential.permanent_delegate;

    // A friend the user might try to move funds to.
    let friend = Keypair::new();
    let friend_account = onboard_confidential(&remit, &friend).await;
    let friend_keys = transfer_keys(&remit, friend_account.elgamal.pubkey()).await;

    remit
        .token
        .freeze(&account.address, &freeze.pubkey(), &[freeze])
        .await
        .unwrap();

    // The owner can no longer move anything out...
    assert_token_error(
        confidential::withdraw(
            &remit.token,
            &payer,
            &account,
            &sanctioned.pubkey(),
            &[&sanctioned],
            ONE,
            DECIMALS,
        )
        .await,
        TokenError::AccountFrozen,
    );
    assert_token_error(
        confidential::transfer_with_fee(
            &remit.token,
            &payer,
            &account,
            &friend_account.address,
            &sanctioned.pubkey(),
            &[&sanctioned],
            ONE,
            &friend_keys,
        )
        .await
        .map(|_| ()),
        TokenError::AccountFrozen,
    );
    assert_token_error(
        confidential::apply_pending(&remit.token, &account, &sanctioned).await,
        TokenError::AccountFrozen,
    );
    assert_token_error(
        confidential::deposit(&remit.token, &account, &sanctioned, 0, DECIMALS).await,
        TokenError::AccountFrozen,
    );

    // ...nothing can come in (a frozen destination rejects transfers)...
    fund_confidential(&remit, &friend_account, &friend, 10 * ONE).await;
    let back_keys = transfer_keys(&remit, account.elgamal.pubkey()).await;
    assert_token_error(
        confidential::transfer_with_fee(
            &remit.token,
            &payer,
            &friend_account,
            &account.address,
            &friend.pubkey(),
            &[&friend],
            ONE,
            &back_keys,
        )
        .await
        .map(|_| ()),
        TokenError::AccountFrozen,
    );

    // ...and the permanent delegate is blocked by the freeze too.
    assert_token_error(
        remit
            .token
            .burn(&account.address, &delegate.pubkey(), ONE, &[delegate])
            .await,
        TokenError::AccountFrozen,
    );

    // The encrypted balance is untouched and still only the owner can read it.
    let b = confidential::balances(&remit.token, &account)
        .await
        .unwrap();
    assert_eq!(
        b,
        Balances {
            public: 0,
            pending: 0,
            available: 500 * ONE
        }
    );
}

#[tokio::test]
async fn funds_are_back_in_reach_only_after_the_owner_withdraws() {
    let Scene {
        env,
        remit,
        sanctioned,
        account,
        seized,
        ..
    } = scene().await;
    let delegate = &remit.confidential.permanent_delegate;

    // Only the owner, with the owner's keys, can turn confidential value
    // back into a public amount.
    confidential::withdraw(
        &remit.token,
        &env.payer.pubkey(),
        &account,
        &sanctioned.pubkey(),
        &[&sanctioned],
        200 * ONE,
        DECIMALS,
    )
    .await
    .unwrap();

    // That public part is immediately seizable.
    remit
        .transfer(
            &account.address,
            &seized,
            &delegate.pubkey(),
            &[delegate],
            150 * ONE,
        )
        .await
        .unwrap();
    remit
        .token
        .burn(&account.address, &delegate.pubkey(), 50 * ONE, &[delegate])
        .await
        .unwrap();

    let b = confidential::balances(&remit.token, &account)
        .await
        .unwrap();
    assert_eq!(
        b,
        Balances {
            public: 0,
            pending: 0,
            available: 300 * ONE
        }
    );
    assert_eq!(state::supply(&remit.token).await, 450 * ONE);
}

#[tokio::test]
async fn auditor_sees_transfer_amounts_but_not_balances() {
    let Scene {
        env,
        remit,
        sanctioned,
        account,
        ..
    } = scene().await;
    let friend = Keypair::new();
    let friend_account = onboard_confidential(&remit, &friend).await;
    let keys = transfer_keys(&remit, friend_account.elgamal.pubkey()).await;

    let audit = confidential::transfer_with_fee(
        &remit.token,
        &env.payer.pubkey(),
        &account,
        &friend_account.address,
        &sanctioned.pubkey(),
        &[&sanctioned],
        120 * ONE,
        &keys,
    )
    .await
    .unwrap();
    assert_eq!(audit.decrypt(&remit.confidential.auditor), Some(120 * ONE));

    // Balances are encrypted to the owner's ElGamal key, not the auditor's.
    let state = confidential::raw_state(&remit.token, &account.address)
        .await
        .unwrap();
    let owner_key: solana_zk_sdk_pod::encryption::elgamal::PodElGamalPubkey =
        (*account.elgamal.pubkey()).into();
    let auditor_key: solana_zk_sdk_pod::encryption::elgamal::PodElGamalPubkey =
        (*remit.confidential.auditor.pubkey()).into();
    assert_eq!(state.elgamal_pubkey, owner_key);
    assert_ne!(state.elgamal_pubkey, auditor_key);

    // The owner reads 380 left; the auditor key decrypts nothing.
    let available: ElGamalCiphertext = state.available_balance.try_into().unwrap();
    assert_eq!(
        account.elgamal.secret().decrypt_u32(&available),
        Some(380 * ONE)
    );
    assert_eq!(
        remit.confidential.auditor.secret().decrypt_u32(&available),
        None
    );
}

#[tokio::test]
async fn mint_cannot_close_while_value_sits_in_confidential_balances() {
    let Scene {
        env,
        remit,
        sanctioned,
        account,
        ..
    } = scene().await;
    let close = &remit.roles.close_authority;
    assert_token_error(
        remit
            .token
            .close_account(&remit.mint, &env.payer.pubkey(), &close.pubkey(), &[close])
            .await,
        TokenError::MintHasSupply,
    );
    // The user's account cannot be closed with a confidential balance either.
    assert_token_error(
        remit
            .token
            .close_account(
                &account.address,
                &sanctioned.pubkey(),
                &sanctioned.pubkey(),
                &[&sanctioned],
            )
            .await,
        TokenError::ConfidentialTransferAccountHasBalance,
    );
}

#[tokio::test]
async fn credit_switches_belong_to_the_owner_not_the_issuer() {
    let Scene {
        remit,
        sanctioned,
        account,
        ..
    } = scene().await;
    let freeze = &remit.roles.freeze_authority;
    let ct_authority = &remit.confidential.confidential_authority;

    // Issuer-side keys cannot flip the account's switches.
    for key in [freeze, ct_authority] {
        assert_token_error(
            remit
                .token
                .confidential_transfer_disable_non_confidential_credits(
                    &account.address,
                    &key.pubkey(),
                    &[key],
                )
                .await,
            TokenError::OwnerMismatch,
        );
    }

    // The owner can refuse public credits: public transfers in now fail...
    remit
        .token
        .confidential_transfer_disable_non_confidential_credits(
            &account.address,
            &sanctioned.pubkey(),
            &[&sanctioned],
        )
        .await
        .unwrap();
    let sender = Keypair::new();
    let sender_account = remit.open_account(&sender.pubkey()).await.unwrap();
    remit.kyc_approve(&sender_account).await.unwrap();
    remit.mint_to(&sender_account, 5 * ONE).await.unwrap();
    assert_token_error(
        remit
            .transfer(
                &sender_account,
                &account.address,
                &sender.pubkey(),
                &[&sender],
                ONE,
            )
            .await,
        TokenError::NonConfidentialTransfersDisabled,
    );
    // ...but the switch does not stop the mint authority from minting in.
    remit.mint_to(&account.address, ONE).await.unwrap();
    assert_eq!(state::amount(&remit.token, &account.address).await, ONE);
}

#[tokio::test]
async fn approval_gates_entry_and_cannot_be_granted_by_others() {
    let env = Env::start().await;
    let remit = RemitV2::create(&env).await.unwrap();
    let user = Keypair::new();
    let account = remit.open_account(&user.pubkey()).await.unwrap();
    remit.kyc_approve(&account).await.unwrap();
    let account = confidential::configure(&remit.token, &account, &user)
        .await
        .unwrap();
    remit.mint_to(&account.address, 20 * ONE).await.unwrap();

    // Unapproved: the user cannot hide funds in a confidential balance.
    assert_token_error(
        confidential::deposit(&remit.token, &account, &user, 20 * ONE, DECIMALS).await,
        TokenError::ConfidentialTransferAccountNotApproved,
    );
    // Neither the user nor the freeze authority can approve it.
    for key in [&user, &remit.roles.freeze_authority] {
        assert_instruction_error(
            remit
                .token
                .confidential_transfer_approve_account(&account.address, &key.pubkey(), &[key])
                .await,
            InstructionError::MissingRequiredSignature,
        );
    }
    // So the public balance stays seizable.
    let delegate = &remit.confidential.permanent_delegate;
    remit
        .token
        .burn(&account.address, &delegate.pubkey(), 20 * ONE, &[delegate])
        .await
        .unwrap();
}

#[tokio::test]
async fn approval_is_one_way_and_auto_approve_only_affects_new_configs() {
    let env = Env::start().await;
    let remit = RemitV2::create(&env).await.unwrap();
    let ct_authority = &remit.confidential.confidential_authority;
    let auditor = Some((*remit.confidential.auditor.pubkey()).into());

    let early = Keypair::new();
    let early_account = remit.open_account(&early.pubkey()).await.unwrap();
    remit.kyc_approve(&early_account).await.unwrap();
    confidential::configure(&remit.token, &early_account, &early)
        .await
        .unwrap();
    assert!(!bool::from(
        confidential::raw_state(&remit.token, &early_account)
            .await
            .unwrap()
            .approved
    ));

    // Turning auto-approve on does not touch accounts configured before.
    remit
        .token
        .confidential_transfer_update_mint(&ct_authority.pubkey(), true, auditor, &[ct_authority])
        .await
        .unwrap();
    assert!(!bool::from(
        confidential::raw_state(&remit.token, &early_account)
            .await
            .unwrap()
            .approved
    ));

    // An account configured now is approved at configure time.
    let late = Keypair::new();
    let late_account = remit.open_account(&late.pubkey()).await.unwrap();
    remit.kyc_approve(&late_account).await.unwrap();
    confidential::configure(&remit.token, &late_account, &late)
        .await
        .unwrap();
    assert!(bool::from(
        confidential::raw_state(&remit.token, &late_account)
            .await
            .unwrap()
            .approved
    ));

    // Turning it off again does not un-approve anyone: there is no
    // "revoke approval" instruction in the confidential transfer extension.
    remit
        .token
        .confidential_transfer_update_mint(&ct_authority.pubkey(), false, auditor, &[ct_authority])
        .await
        .unwrap();
    assert!(bool::from(
        confidential::raw_state(&remit.token, &late_account)
            .await
            .unwrap()
            .approved
    ));
}

#[tokio::test]
async fn public_balance_can_go_confidential_in_a_single_transaction() {
    use spl_token_2022_interface::extension::confidential_transfer::instruction as ct;

    let env = Env::start().await;
    let remit = RemitV2::create(&env).await.unwrap();
    let user = Keypair::new();
    let account = onboard_confidential(&remit, &user).await;
    remit.mint_to(&account.address, 60 * ONE).await.unwrap();

    // Deposit and ApplyPendingBalance need no ZK proofs, so the owner can put
    // both in one transaction: public -> available in a single slot.
    let before = confidential::raw_state(&remit.token, &account.address)
        .await
        .unwrap();
    let counter = u64::from(before.pending_balance_credit_counter) + 1;
    let program = spl_token_2022_interface::id();
    let deposit = ct::deposit(
        &program,
        &account.address,
        &remit.mint,
        60 * ONE,
        DECIMALS,
        &user.pubkey(),
        &[],
    )
    .unwrap();
    let apply = ct::apply_pending_balance(
        &program,
        &account.address,
        counter,
        &account.aes.encrypt(60 * ONE).into(),
        &user.pubkey(),
        &[],
    )
    .unwrap();
    remit
        .token
        .process_ixs(&[deposit, apply], &[&user])
        .await
        .unwrap();

    let b = confidential::balances(&remit.token, &account)
        .await
        .unwrap();
    assert_eq!(
        b,
        Balances {
            public: 0,
            pending: 0,
            available: 60 * ONE
        }
    );
    let delegate = &remit.confidential.permanent_delegate;
    assert_token_error(
        remit
            .token
            .burn(&account.address, &delegate.pubkey(), ONE, &[delegate])
            .await,
        TokenError::InsufficientFunds,
    );
}
