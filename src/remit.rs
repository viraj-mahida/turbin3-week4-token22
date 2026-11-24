//! The two versions of the remittance mint.
//!
//! Every privileged action is held by a separate key so the tests can show
//! exactly which role is allowed to do what.
//!
//! The mint is created from an explicit instruction list ([`plan_mint`]):
//! `CreateAccount` sized with `ExtensionType::try_calculate_account_len`,
//! then every extension initializer, then `InitializeMint2`, then the
//! variable-length `TokenMetadata` (which the metadata interface requires to
//! come after the mint exists). All in one transaction.

use {
    crate::{
        env::{Env, RemitToken},
        state,
    },
    solana_instruction::Instruction,
    solana_keypair::Keypair,
    solana_pubkey::Pubkey,
    solana_rent::Rent,
    solana_signer::{signers::Signers, Signer},
    solana_zk_sdk::encryption::elgamal::ElGamalKeypair,
    spl_token_2022_interface::{
        extension::{
            confidential_transfer, confidential_transfer_fee, default_account_state,
            metadata_pointer::{self, MetadataPointer},
            transfer_fee::{self, TransferFeeConfig},
            BaseStateWithExtensions, ExtensionType,
        },
        instruction::{
            initialize_mint2, initialize_mint_close_authority, initialize_permanent_delegate,
        },
        state::{AccountState, Mint},
    },
    spl_token_client::token::{TokenError, TokenResult},
    spl_token_metadata_interface::state::{Field, TokenMetadata},
    spl_type_length_value::variable_len_pack::VariableLenPack,
};

/// Six decimals, like most fiat-backed stablecoins.
pub const DECIMALS: u8 = 6;
/// One whole token in base units.
pub const ONE: u64 = 1_000_000;
/// Initial fee schedule: 0.50% on every transfer...
pub const FEE_BASIS_POINTS: u16 = 50;
/// ...capped at 5 tokens, so large remittances are not penalised.
pub const MAXIMUM_FEE: u64 = 5 * ONE;

pub const NAME: &str = "Remit Dollar";
pub const SYMBOL: &str = "RMTD";
pub const URI: &str = "https://example.com/rmtd/metadata.json";
pub const PEG_KEY: &str = "peg";
pub const PEG_VALUE: &str = "USD";

/// Keys that run the public (v1) mint.
pub struct Roles {
    /// Issuer: mints against fiat received off chain.
    pub mint_authority: Keypair,
    /// Compliance desk: thaws accounts after KYC, freezes them on a hit.
    pub freeze_authority: Keypair,
    /// Can change the transfer fee (takes effect two epochs later).
    pub fee_config_authority: Keypair,
    /// Fee desk: collects the withheld transfer fees.
    pub withdraw_withheld_authority: Keypair,
    /// Can edit the on-mint metadata and the metadata pointer.
    pub metadata_authority: Keypair,
    /// Can close the mint once the supply is back to zero.
    pub close_authority: Keypair,
}

impl Roles {
    pub fn generate() -> Self {
        Self {
            mint_authority: Keypair::new(),
            freeze_authority: Keypair::new(),
            fee_config_authority: Keypair::new(),
            withdraw_withheld_authority: Keypair::new(),
            metadata_authority: Keypair::new(),
            close_authority: Keypair::new(),
        }
    }
}

/// Extra keys for the confidential (v2) mint.
pub struct ConfidentialRoles {
    /// Seizure authority: can transfer or burn from any account of this mint.
    pub permanent_delegate: Keypair,
    /// Approves accounts for confidential transfers (approve policy: manual).
    pub confidential_authority: Keypair,
    /// Every confidential transfer amount is also encrypted to this key.
    pub auditor: ElGamalKeypair,
    /// Can change the confidential fee settings.
    pub confidential_fee_authority: Keypair,
    /// Confidential transfer fees are encrypted to this key, so only the fee
    /// desk can decrypt and withdraw them.
    pub withdraw_withheld_elgamal: ElGamalKeypair,
}

impl ConfidentialRoles {
    pub fn generate() -> Self {
        Self {
            permanent_delegate: Keypair::new(),
            confidential_authority: Keypair::new(),
            auditor: ElGamalKeypair::new_rand(),
            confidential_fee_authority: Keypair::new(),
            withdraw_withheld_elgamal: ElGamalKeypair::new_rand(),
        }
    }
}

/// v1 extensions, in the order their initializers are sent.
pub const V1_EXTENSIONS: [ExtensionType; 4] = [
    ExtensionType::TransferFeeConfig,
    ExtensionType::MetadataPointer,
    ExtensionType::DefaultAccountState,
    ExtensionType::MintCloseAuthority,
];

/// v2 carries the whole v1 set forward and adds seizure + confidentiality.
pub const V2_EXTENSIONS: [ExtensionType; 7] = [
    ExtensionType::TransferFeeConfig,
    ExtensionType::MetadataPointer,
    ExtensionType::DefaultAccountState,
    ExtensionType::MintCloseAuthority,
    ExtensionType::PermanentDelegate,
    ExtensionType::ConfidentialTransferMint,
    ExtensionType::ConfidentialTransferFeeConfig,
];

/// The metadata the mint ends up with (used for sizing before it exists).
pub fn final_metadata(mint: &Pubkey) -> TokenMetadata {
    TokenMetadata {
        mint: *mint,
        name: NAME.to_string(),
        symbol: SYMBOL.to_string(),
        uri: URI.to_string(),
        additional_metadata: vec![(PEG_KEY.to_string(), PEG_VALUE.to_string())],
        ..Default::default()
    }
}

/// Instruction list and sizing for a new mint.
pub struct MintPlan {
    /// `CreateAccount`, extension inits, `InitializeMint2`, metadata init,
    /// metadata field update, in that order.
    pub instructions: Vec<Instruction>,
    /// Bytes allocated by `CreateAccount`: base mint + fixed-size extensions,
    /// from `ExtensionType::try_calculate_account_len::<Mint>`.
    pub space: usize,
    /// TLV bytes the `TokenMetadata` entry will take once written.
    pub metadata_len: usize,
    /// Rent-exempt minimum for `space + metadata_len`, paid up front so the
    /// metadata instructions can grow the account without extra transfers.
    pub lamports: u64,
}

/// Bytes a `TokenMetadata` entry takes inside a Token-2022 mint: a 2-byte
/// extension type, a 2-byte length, then the Borsh-packed value.
///
/// `TokenMetadata::tlv_size_of` is not used here: it measures a generic
/// spl-type-length-value entry (8-byte discriminator + 4-byte length), which
/// is 8 bytes larger than the entry Token-2022 actually writes.
pub fn metadata_entry_len(metadata: &TokenMetadata) -> TokenResult<usize> {
    let value = metadata.get_packed_len()?;
    Ok(std::mem::size_of::<ExtensionType>() + std::mem::size_of::<u16>() + value)
}

/// Builds the creation instructions for v1 (`confidential = None`) or v2.
pub fn plan_mint(
    payer: &Pubkey,
    mint: &Pubkey,
    roles: &Roles,
    confidential: Option<(&ConfidentialRoles, bool)>,
    rent: &Rent,
) -> TokenResult<MintPlan> {
    let program = spl_token_2022_interface::id();
    let extensions: &[ExtensionType] = if confidential.is_some() {
        &V2_EXTENSIONS
    } else {
        &V1_EXTENSIONS
    };
    let space = ExtensionType::try_calculate_account_len::<Mint>(extensions)?;
    let metadata_len = metadata_entry_len(&final_metadata(mint))?;
    let lamports = rent.minimum_balance(space + metadata_len);

    let mut instructions = vec![
        solana_system_interface::instruction::create_account(
            payer,
            mint,
            lamports,
            space as u64,
            &program,
        ),
        transfer_fee::instruction::initialize_transfer_fee_config(
            &program,
            mint,
            Some(&roles.fee_config_authority.pubkey()),
            Some(&roles.withdraw_withheld_authority.pubkey()),
            FEE_BASIS_POINTS,
            MAXIMUM_FEE,
        )?,
        // The metadata lives in the mint account itself.
        metadata_pointer::instruction::initialize(
            &program,
            mint,
            Some(roles.metadata_authority.pubkey()),
            Some(*mint),
        )?,
        default_account_state::instruction::initialize_default_account_state(
            &program,
            mint,
            &AccountState::Frozen,
        )?,
        initialize_mint_close_authority(&program, mint, Some(&roles.close_authority.pubkey()))?,
    ];

    if let Some((c, auto_approve_new_accounts)) = confidential {
        instructions.push(initialize_permanent_delegate(
            &program,
            mint,
            &c.permanent_delegate.pubkey(),
        )?);
        instructions.push(confidential_transfer::instruction::initialize_mint(
            &program,
            mint,
            Some(c.confidential_authority.pubkey()),
            auto_approve_new_accounts,
            Some((*c.auditor.pubkey()).into()),
        )?);
        // Required when a mint has both a transfer fee and confidential
        // transfers: fees on confidential transfers are withheld encrypted.
        instructions.push(
            confidential_transfer_fee::instruction::initialize_confidential_transfer_fee_config(
                &program,
                mint,
                Some(c.confidential_fee_authority.pubkey()),
                &(*c.withdraw_withheld_elgamal.pubkey()).into(),
            )?,
        );
    }

    instructions.push(initialize_mint2(
        &program,
        mint,
        &roles.mint_authority.pubkey(),
        Some(&roles.freeze_authority.pubkey()),
        DECIMALS,
    )?);

    // TokenMetadata is variable length and can only be written once the mint
    // is initialized; the account was pre-funded for its final size.
    instructions.push(spl_token_metadata_interface::instruction::initialize(
        &program,
        mint,
        &roles.metadata_authority.pubkey(),
        mint,
        &roles.mint_authority.pubkey(),
        NAME.to_string(),
        SYMBOL.to_string(),
        URI.to_string(),
    ));
    instructions.push(spl_token_metadata_interface::instruction::update_field(
        &program,
        mint,
        &roles.metadata_authority.pubkey(),
        Field::Key(PEG_KEY.to_string()),
        PEG_VALUE.to_string(),
    ));

    Ok(MintPlan {
        instructions,
        space,
        metadata_len,
        lamports,
    })
}

async fn create_mint(
    env: &Env,
    token: &RemitToken,
    mint: &Keypair,
    roles: &Roles,
    confidential: Option<(&ConfidentialRoles, bool)>,
) -> TokenResult<()> {
    let rent = env.rent().await;
    let plan = plan_mint(
        &env.payer_pubkey(),
        &mint.pubkey(),
        roles,
        confidential,
        &rent,
    )?;
    token
        .process_ixs(
            &plan.instructions,
            &[mint, &roles.mint_authority, &roles.metadata_authority],
        )
        .await?;
    Ok(())
}

/// Fee for `amount` under the schedule in force *now*: reads the Clock
/// sysvar for the epoch and the mint's `TransferFeeConfig`, then calls
/// `calculate_epoch_fee(epoch, amount)`. No rate is cached.
pub async fn current_fee(env: &Env, token: &RemitToken, amount: u64) -> TokenResult<u64> {
    let epoch = env.clock().await.epoch;
    let mint = state::read(token, token.get_address()).await?;
    let config = *mint.mint().get_extension::<TransferFeeConfig>()?;
    config
        .calculate_epoch_fee(epoch, amount)
        .ok_or(TokenError::Program(
            spl_token_2022_interface::error::TokenError::Overflow.into(),
        ))
}

/// `(basis points, maximum fee)` in force now. Confidential transfers must
/// prove the fee against exactly these parameters.
pub async fn current_fee_parameters(env: &Env, token: &RemitToken) -> TokenResult<(u16, u64)> {
    let epoch = env.clock().await.epoch;
    let mint = state::read(token, token.get_address()).await?;
    let config = *mint.mint().get_extension::<TransferFeeConfig>()?;
    let fee = config.get_epoch_fee(epoch);
    Ok((
        u16::from(fee.transfer_fee_basis_points),
        u64::from(fee.maximum_fee),
    ))
}

/// The only public transfer path in this repo: `TransferCheckedWithFee`,
/// with the fee computed from the current epoch. Returns the fee charged.
pub async fn transfer<S: Signers>(
    env: &Env,
    token: &RemitToken,
    source: &Pubkey,
    destination: &Pubkey,
    authority: &Pubkey,
    signers: &S,
    amount: u64,
) -> TokenResult<u64> {
    let fee = current_fee(env, token, amount).await?;
    transfer_with_fee_unchecked(token, source, destination, authority, signers, amount, fee)
        .await?;
    Ok(fee)
}

/// `TransferCheckedWithFee` with a caller-chosen fee. Only used by tests to
/// show the program rejects a fee that does not match the epoch's schedule.
pub async fn transfer_with_fee_unchecked<S: Signers>(
    token: &RemitToken,
    source: &Pubkey,
    destination: &Pubkey,
    authority: &Pubkey,
    signers: &S,
    amount: u64,
    fee: u64,
) -> TokenResult<()> {
    let ix = transfer_fee::instruction::transfer_checked_with_fee(
        &spl_token_2022_interface::id(),
        source,
        token.get_address(),
        destination,
        authority,
        &[],
        amount,
        DECIMALS,
        fee,
    )?;
    token.process_ixs(&[ix], signers).await?;
    Ok(())
}

/// Part 1: the public remittance mint.
pub struct RemitV1 {
    pub env: Env,
    pub mint: Pubkey,
    pub token: RemitToken,
    pub roles: Roles,
}

impl RemitV1 {
    pub const EXTENSIONS: [ExtensionType; 4] = V1_EXTENSIONS;

    pub async fn create(env: &Env) -> TokenResult<Self> {
        let roles = Roles::generate();
        let mint_keypair = Keypair::new();
        let mint = mint_keypair.pubkey();
        let token = env.token(&mint, DECIMALS);
        create_mint(env, &token, &mint_keypair, &roles, None).await?;
        Ok(Self {
            env: env.clone(),
            mint,
            token,
            roles,
        })
    }

    /// Opens the owner's associated token account. Because of
    /// `DefaultAccountState::Frozen` it starts frozen.
    pub async fn open_account(&self, owner: &Pubkey) -> TokenResult<Pubkey> {
        self.token.create_associated_token_account(owner).await?;
        Ok(self.token.get_associated_token_address(owner))
    }

    /// KYC passed: the compliance desk thaws this one account. The mint's
    /// default state is not touched.
    pub async fn kyc_approve(&self, account: &Pubkey) -> TokenResult<()> {
        thaw(&self.token, &self.roles, account).await
    }

    pub async fn mint_to(&self, account: &Pubkey, amount: u64) -> TokenResult<()> {
        mint_to(&self.token, &self.roles, account, amount).await
    }

    /// See [`transfer`].
    pub async fn transfer<S: Signers>(
        &self,
        source: &Pubkey,
        destination: &Pubkey,
        authority: &Pubkey,
        signers: &S,
        amount: u64,
    ) -> TokenResult<u64> {
        transfer(
            &self.env,
            &self.token,
            source,
            destination,
            authority,
            signers,
            amount,
        )
        .await
    }
}

/// Part 2: v1 re-issued with a permanent delegate and confidential transfers.
pub struct RemitV2 {
    pub env: Env,
    pub mint: Pubkey,
    pub token: RemitToken,
    pub roles: Roles,
    pub confidential: ConfidentialRoles,
}

impl RemitV2 {
    pub const EXTENSIONS: [ExtensionType; 7] = V2_EXTENSIONS;

    /// Approve policy: manual (`auto_approve_new_accounts = false`). An
    /// account can only use confidential transfers after the confidential
    /// authority approves it.
    pub async fn create(env: &Env) -> TokenResult<Self> {
        Self::create_with(env, false).await
    }

    pub async fn create_with(env: &Env, auto_approve_new_accounts: bool) -> TokenResult<Self> {
        let roles = Roles::generate();
        let confidential = ConfidentialRoles::generate();
        let mint_keypair = Keypair::new();
        let mint = mint_keypair.pubkey();
        let token = env.token(&mint, DECIMALS);
        create_mint(
            env,
            &token,
            &mint_keypair,
            &roles,
            Some((&confidential, auto_approve_new_accounts)),
        )
        .await?;
        Ok(Self {
            env: env.clone(),
            mint,
            token,
            roles,
            confidential,
        })
    }

    pub async fn open_account(&self, owner: &Pubkey) -> TokenResult<Pubkey> {
        self.token.create_associated_token_account(owner).await?;
        Ok(self.token.get_associated_token_address(owner))
    }

    pub async fn kyc_approve(&self, account: &Pubkey) -> TokenResult<()> {
        thaw(&self.token, &self.roles, account).await
    }

    /// The confidential authority approves a configured account.
    pub async fn approve_confidential(&self, account: &Pubkey) -> TokenResult<()> {
        let authority = &self.confidential.confidential_authority;
        self.token
            .confidential_transfer_approve_account(account, &authority.pubkey(), &[authority])
            .await?;
        Ok(())
    }

    pub async fn mint_to(&self, account: &Pubkey, amount: u64) -> TokenResult<()> {
        mint_to(&self.token, &self.roles, account, amount).await
    }

    /// See [`transfer`].
    pub async fn transfer<S: Signers>(
        &self,
        source: &Pubkey,
        destination: &Pubkey,
        authority: &Pubkey,
        signers: &S,
        amount: u64,
    ) -> TokenResult<u64> {
        transfer(
            &self.env,
            &self.token,
            source,
            destination,
            authority,
            signers,
            amount,
        )
        .await
    }
}

async fn thaw(token: &RemitToken, roles: &Roles, account: &Pubkey) -> TokenResult<()> {
    token
        .thaw(
            account,
            &roles.freeze_authority.pubkey(),
            &[&roles.freeze_authority],
        )
        .await?;
    Ok(())
}

async fn mint_to(
    token: &RemitToken,
    roles: &Roles,
    account: &Pubkey,
    amount: u64,
) -> TokenResult<()> {
    token
        .mint_to(
            account,
            &roles.mint_authority.pubkey(),
            amount,
            &[&roles.mint_authority],
        )
        .await?;
    Ok(())
}

/// Reads the metadata stored inside the mint and the pointer that leads to it.
pub async fn read_metadata(token: &RemitToken) -> TokenResult<(MetadataPointer, TokenMetadata)> {
    let raw = state::read(token, token.get_address()).await?;
    let mint = raw.mint();
    let pointer = *mint.get_extension::<MetadataPointer>()?;
    let metadata = mint.get_variable_len_extension::<TokenMetadata>()?;
    Ok((pointer, metadata))
}
