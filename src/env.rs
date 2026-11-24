//! Local chain used by the tests: `solana-program-test` with the Token-2022
//! SBF program loaded from `tests/fixtures/spl_token_2022.so` and the
//! `remit-agent` program from `tests/fixtures/remit_agent.so`.
//!
//! The ZK ElGamal proof program is a builtin of the bank, so confidential
//! transfer proofs are verified on chain exactly as on a real cluster.

use {
    crate::state::RawAccount,
    solana_clock::Clock,
    solana_keypair::Keypair,
    solana_program_test::{tokio::sync::Mutex, ProgramTest, ProgramTestContext},
    solana_pubkey::Pubkey,
    solana_rent::Rent,
    solana_signer::Signer,
    spl_token_client::{
        client::{ProgramBanksClient, ProgramBanksClientProcessTransaction, ProgramClient},
        token::{ComputeUnitLimit, Token},
    },
    std::sync::Arc,
};

/// Token handle bound to the local bank.
pub type RemitToken = Token<ProgramBanksClientProcessTransaction>;

#[derive(Clone)]
pub struct Env {
    pub context: Arc<Mutex<ProgramTestContext>>,
    pub client: Arc<dyn ProgramClient<ProgramBanksClientProcessTransaction>>,
    pub payer: Arc<Keypair>,
}

impl Env {
    /// Starts a fresh bank with Token-2022 deployed at its canonical address
    /// and the remit-agent program at [`crate::agent::ID`].
    pub async fn start() -> Self {
        let mut program_test = ProgramTest::default();
        program_test.prefer_bpf(true);
        program_test.add_program("spl_token_2022", spl_token_2022_interface::id(), None);
        program_test.add_program("remit_agent", crate::agent::ID, None);
        let context = program_test.start_with_context().await;
        let payer = Arc::new(clone_keypair(&context.payer));
        let context = Arc::new(Mutex::new(context));
        let client: Arc<dyn ProgramClient<ProgramBanksClientProcessTransaction>> =
            Arc::new(ProgramBanksClient::new_from_context(
                Arc::clone(&context),
                ProgramBanksClientProcessTransaction,
            ));
        Self {
            context,
            client,
            payer,
        }
    }

    /// Token client for `mint`, paying fees from the bank's payer.
    pub fn token(&self, mint: &Pubkey, decimals: u8) -> RemitToken {
        Token::new(
            Arc::clone(&self.client),
            &spl_token_2022_interface::id(),
            mint,
            Some(decimals),
            Arc::clone(&self.payer) as _,
        )
        .with_compute_unit_limit(ComputeUnitLimit::Simulated)
    }

    pub fn payer_pubkey(&self) -> Pubkey {
        self.payer.pubkey()
    }

    /// Current `Clock` sysvar, read from the bank.
    pub async fn clock(&self) -> Clock {
        let banks = self.context.lock().await.banks_client.clone();
        banks.get_sysvar::<Clock>().await.expect("clock sysvar")
    }

    /// Current `Rent` sysvar, read from the bank.
    pub async fn rent(&self) -> Rent {
        let banks = self.context.lock().await.banks_client.clone();
        banks.get_rent().await.expect("rent sysvar")
    }

    /// Jumps the bank forward to the first slot of `epoch`.
    pub async fn warp_to_epoch(&self, epoch: u64) {
        self.context
            .lock()
            .await
            .warp_to_epoch(epoch)
            .expect("warp to epoch");
    }

    /// Raw account bytes; decode them with [`RawAccount::mint`] or
    /// [`RawAccount::token_account`] (both `StateWithExtensions::unpack`).
    pub async fn read(&self, address: &Pubkey) -> RawAccount {
        self.try_read(address)
            .await
            .unwrap_or_else(|| panic!("account {address} not found"))
    }

    pub async fn try_read(&self, address: &Pubkey) -> Option<RawAccount> {
        self.client
            .get_account(*address)
            .await
            .expect("get account")
            .map(|account| RawAccount {
                lamports: account.lamports,
                data: account.data,
            })
    }
}

pub fn clone_keypair(keypair: &Keypair) -> Keypair {
    Keypair::new_from_array(*keypair.secret_bytes())
}
