# Technical notes: sanctions vs. confidential balances

These are facts for the written question: *what happens if a sanctioned user moves funds into a confidential balance before the permanent delegate acts?*

- **Evidence:** each fact points to the test that shows it (in `tests/`) and to the Token-2022 source at the commit this repo builds (`solana-program/token-2022@1d91cfc`).
- **Unconfirmed points:** a fact without a test is labelled *(code reading)*.
- **Setup used by the tests:**
  - a v2 RMTD account, KYC'd and approved, holding 500 RMTD
  - all 500 moved to the confidential **available** balance (`deposit` then `apply_pending`), so its public `amount` is 0

## 1. Where the value sits

- **A token account has three balances:**
  - the public `amount`
  - an encrypted **pending** balance (`pending_balance_lo` / `_hi`)
  - an encrypted **available** balance, plus an AES copy that only the owner can read ([`ConfidentialTransferAccount`](https://github.com/solana-program/token-2022/blob/1d91cfca9395a24adcdc453b38cb776d9624ff8e/interface/src/extension/confidential_transfer/mod.rs#L101))
- **Deposit moves value out of `amount`:** `Deposit` subtracts from `amount` and adds the same value to the pending ciphertext ([processor L465-L494](https://github.com/solana-program/token-2022/blob/1d91cfca9395a24adcdc453b38cb776d9624ff8e/program/src/extension/confidential_transfer/processor.rs#L465-L494)).
- **Supply doesn't change:** tokens held confidentially still count in the mint's `supply`.
  - Tests: `delegate_cannot_burn_or_transfer_a_confidential_balance`, `full_confidential_lifecycle_with_confidential_fee`
- **The mint can't be closed** while confidential balances exist (`MintHasSupply`), and an account holding a confidential balance can't be closed (`ConfidentialTransferAccountHasBalance`).
  - Test: `mint_cannot_close_while_value_sits_in_confidential_balances`
  - Code: [mint close supply check](https://github.com/solana-program/token-2022/blob/1d91cfca9395a24adcdc453b38cb776d9624ff8e/program/src/processor.rs#L1419), [account closable check](https://github.com/solana-program/token-2022/blob/1d91cfca9395a24adcdc453b38cb776d9624ff8e/program/src/processor.rs#L1392-L1396)
- **Going confidential can take one transaction:** `Deposit` and `ApplyPendingBalance` need no ZK proofs, so the owner can do both in one transaction. Public becomes available in a single slot.
  - Test: `public_balance_can_go_confidential_in_a_single_transaction`

## 2. Permanent delegate

- **What it reaches:** the delegate is accepted as an authority by the regular `Transfer`/`TransferChecked` and `Burn`/`BurnChecked` instructions ([transfer](https://github.com/solana-program/token-2022/blob/1d91cfca9395a24adcdc453b38cb776d9624ff8e/program/src/processor.rs#L486), [burn](https://github.com/solana-program/token-2022/blob/1d91cfca9395a24adcdc453b38cb776d9624ff8e/program/src/processor.rs#L1277)). Those instructions only debit the public `amount` ([transfer amount check](https://github.com/solana-program/token-2022/blob/1d91cfca9395a24adcdc453b38cb776d9624ff8e/program/src/processor.rs#L390-L393), [burn amount check](https://github.com/solana-program/token-2022/blob/1d91cfca9395a24adcdc453b38cb776d9624ff8e/program/src/processor.rs#L1236-L1238)).
  - Test: `permanent_delegate_can_seize_and_burn_a_public_balance`
- **It fails on confidential balances:** with the public `amount` at 0, a delegate burn or transfer of even 1 base unit fails with `InsufficientFunds`. This holds for the available balance and for a balance still pending.
  - Tests: `delegate_cannot_burn_or_transfer_a_confidential_balance`, `delegate_cannot_touch_a_pending_balance_either`
- **The confidential instructions only accept the owner.** `Deposit`, `Withdraw`, `Transfer`/`TransferWithFee` and `ApplyPendingBalance` all call `validate_owner` against `token_account.owner` and nothing else. There is no delegate or permanent-delegate branch.
  - Code: [deposit L444](https://github.com/solana-program/token-2022/blob/1d91cfca9395a24adcdc453b38cb776d9624ff8e/program/src/extension/confidential_transfer/processor.rs#L444), [withdraw L568](https://github.com/solana-program/token-2022/blob/1d91cfca9395a24adcdc453b38cb776d9624ff8e/program/src/extension/confidential_transfer/processor.rs#L568), [transfer source L866](https://github.com/solana-program/token-2022/blob/1d91cfca9395a24adcdc453b38cb776d9624ff8e/program/src/extension/confidential_transfer/processor.rs#L866), [transfer-with-fee source L1013](https://github.com/solana-program/token-2022/blob/1d91cfca9395a24adcdc453b38cb776d9624ff8e/program/src/extension/confidential_transfer/processor.rs#L1013), [apply pending L1211](https://github.com/solana-program/token-2022/blob/1d91cfca9395a24adcdc453b38cb776d9624ff8e/program/src/extension/confidential_transfer/processor.rs#L1211)
  - The CPI-guard comments in those functions mention "the account owner is the permanent delegate". That only covers the case where the delegate key *is* the owner.
- **Valid proofs don't help the delegate.** The test builds fully valid withdraw and transfer proofs with the *user's own* ElGamal/AES keys, then submits them with the permanent delegate as the signing authority. Both fail on chain with `OwnerMismatch`. `ApplyPendingBalance` signed by the delegate also fails with `OwnerMismatch`.
  - Tests: `delegate_cannot_withdraw_or_transfer_confidentially_even_with_valid_proofs`, `delegate_cannot_touch_a_pending_balance_either`
- **The delegate can't decrypt or prove anything** about the balance anyway. The owner's ElGamal/AES keys are derived from the owner's signature over a fixed message (`confidential::derive_keys`), and every proof is checked against the ElGamal pubkey stored in the account ([source pubkey check](https://github.com/solana-program/token-2022/blob/1d91cfca9395a24adcdc453b38cb776d9624ff8e/program/src/extension/confidential_transfer/processor.rs#L901)) *(code reading for the line, behaviour shown by the lifecycle test)*.
- **Confidential burn is owner-only too.** The separate `ConfidentialMintBurn` extension also requires the owner, even with a permissioned-burn approver ([validate_owner L396](https://github.com/solana-program/token-2022/blob/1d91cfca9395a24adcdc453b38cb776d9624ff8e/program/src/extension/confidential_mint_burn/processor.rs#L396)) *(code reading; not used in this repo)*.

## 3. Freeze authority

- **A frozen account is stuck.** Freezing makes every confidential instruction on the account fail with `AccountFrozen`:
  - deposit ([L452](https://github.com/solana-program/token-2022/blob/1d91cfca9395a24adcdc453b38cb776d9624ff8e/program/src/extension/confidential_transfer/processor.rs#L452))
  - withdraw ([L576](https://github.com/solana-program/token-2022/blob/1d91cfca9395a24adcdc453b38cb776d9624ff8e/program/src/extension/confidential_transfer/processor.rs#L576))
  - apply pending ([L1219](https://github.com/solana-program/token-2022/blob/1d91cfca9395a24adcdc453b38cb776d9624ff8e/program/src/extension/confidential_transfer/processor.rs#L1219))
  - transfer out ([L887](https://github.com/solana-program/token-2022/blob/1d91cfca9395a24adcdc453b38cb776d9624ff8e/program/src/extension/confidential_transfer/processor.rs#L887), [L1034](https://github.com/solana-program/token-2022/blob/1d91cfca9395a24adcdc453b38cb776d9624ff8e/program/src/extension/confidential_transfer/processor.rs#L1034))
  - transfer **in**, because the destination is frozen ([L944](https://github.com/solana-program/token-2022/blob/1d91cfca9395a24adcdc453b38cb776d9624ff8e/program/src/extension/confidential_transfer/processor.rs#L944), [L1094](https://github.com/solana-program/token-2022/blob/1d91cfca9395a24adcdc453b38cb776d9624ff8e/program/src/extension/confidential_transfer/processor.rs#L1094))
  - Test: `freeze_locks_a_confidential_balance_in_place_without_seizing_it`
- **A freeze also blocks the delegate:** the delegate's burn or transfer on a frozen account fails with `AccountFrozen`, because the freeze check comes before the authority check ([transfer L387](https://github.com/solana-program/token-2022/blob/1d91cfca9395a24adcdc453b38cb776d9624ff8e/program/src/processor.rs#L387), [burn L1230](https://github.com/solana-program/token-2022/blob/1d91cfca9395a24adcdc453b38cb776d9624ff8e/program/src/processor.rs#L1230)).
  - Tests: `permanent_delegate_cannot_act_on_a_frozen_account`, `freeze_locks_a_confidential_balance_in_place_without_seizing_it`
- **A freeze changes no balances:** after freezing, the encrypted balance is unchanged and the owner can still decrypt 500 RMTD.
- **Thawing gives the delegate nothing here:** for a *public* balance the order is thaw, seize, refreeze (shown in `permanent_delegate_cannot_act_on_a_frozen_account`). For a *confidential* balance, thawing only lets the owner move funds again, because section 2 still applies.

## 4. Withdraw is the only way back

- **Only the owner can bring funds back to public.** The only instruction that turns confidential value back into a public `amount` is `Withdraw`, which needs:
  - the owner's signature
  - an equality proof and a range proof built from the owner's keys
  - an account that isn't frozen
- **Once withdrawn, the funds are seizable straight away:** after the owner withdraws 200, the delegate immediately transfers 150 out and burns 50. The other 300 stays confidential and out of reach.
  - Test: `funds_are_back_in_reach_only_after_the_owner_withdraws`
- **Deposit and withdraw amounts are public.** Both take the amount as a plain `u64` in instruction data ([deposit](https://github.com/solana-program/token-2022/blob/1d91cfca9395a24adcdc453b38cb776d9624ff8e/interface/src/extension/confidential_transfer/instruction.rs#L944-L952), [withdraw](https://github.com/solana-program/token-2022/blob/1d91cfca9395a24adcdc453b38cb776d9624ff8e/interface/src/extension/confidential_transfer/instruction.rs#L1052)). Only confidential *transfer* amounts are encrypted.

## 5. Auditor key

- **It sees transfer amounts.** Every confidential transfer carries the amount encrypted to the auditor's ElGamal key (in the amount-validity proof's third handle). The test decrypts it with the auditor secret and gets exactly 120 RMTD.
  - Test: `auditor_sees_transfer_amounts_but_not_balances`
- **It can't read balances.** Balances are encrypted to the owner's ElGamal key only. Decrypting the available balance with the auditor secret returns nothing (`None`), while the owner's key reads 380.
  - Test: `auditor_sees_transfer_amounts_but_not_balances`
- **It's a read-only key.** It signs nothing and no instruction takes it as an authority.
- **It can be rotated, but only going forward.** The confidential authority can change it with `UpdateMint` ([L83-L112](https://github.com/solana-program/token-2022/blob/1d91cfca9395a24adcdc453b38cb776d9624ff8e/program/src/extension/confidential_transfer/processor.rs#L83-L112)). Only transfers made after the change are encrypted to the new key *(code reading)*.
- **Balances could be rebuilt from history** *(inference, not tested)*. Deposit and withdraw amounts are public and transfer amounts are visible to the auditor. So a full history of an account's transactions from the moment it was configured is enough to work out its confidential balance without the owner's key.

## 6. Confidential transfer authority (approve / auto-approve)

- **Approval gates entry.** With auto-approve off, a configured but unapproved account can't deposit (`ConfidentialTransferAccountNotApproved`). Its value stays public and seizable.
  - Tests: `approval_gates_entry_and_cannot_be_granted_by_others`, `accounts_need_kyc_thaw_and_confidential_approval`
- **Only the confidential authority can approve.** The owner, the freeze authority or anyone else gets `MissingRequiredSignature` ([L310-L343](https://github.com/solana-program/token-2022/blob/1d91cfca9395a24adcdc453b38cb776d9624ff8e/program/src/extension/confidential_transfer/processor.rs#L310-L343)).
- **Approval is one-way.** `ApproveAccount` only sets `approved = true`. The confidential transfer instruction set has no un-approve or revoke ([instruction enum](https://github.com/solana-program/token-2022/blob/1d91cfca9395a24adcdc453b38cb776d9624ff8e/interface/src/extension/confidential_transfer/instruction.rs#L51-L491)).
  - Test: `approval_is_one_way_and_auto_approve_only_affects_new_configs`
- **Auto-approve is read once.** It's copied into the account at `ConfigureAccount` ([L281](https://github.com/solana-program/token-2022/blob/1d91cfca9395a24adcdc453b38cb776d9624ff8e/program/src/extension/confidential_transfer/processor.rs#L281)). Changing it later doesn't affect accounts that are already configured, in either direction.
  - Test: `approval_is_one_way_and_auto_approve_only_affects_new_configs`
- **Approval doesn't let the authority move funds.** It's a separate key from the freeze authority and the delegate.

## 7. Credit switches belong to the owner

- **Only the owner can flip them.** `DisableNonConfidentialCredits` and `DisableConfidentialCredits` (and their enable versions) are owner-signed ([L1262](https://github.com/solana-program/token-2022/blob/1d91cfca9395a24adcdc453b38cb776d9624ff8e/program/src/extension/confidential_transfer/processor.rs#L1262), [L1293](https://github.com/solana-program/token-2022/blob/1d91cfca9395a24adcdc453b38cb776d9624ff8e/program/src/extension/confidential_transfer/processor.rs#L1293)). The freeze authority and the confidential authority get `OwnerMismatch`.
  - Test: `credit_switches_belong_to_the_owner_not_the_issuer`
- **Disabling public credits blocks public transfers in:** with non-confidential credits off, a public transfer *into* the account fails with `NonConfidentialTransfersDisabled` ([L567](https://github.com/solana-program/token-2022/blob/1d91cfca9395a24adcdc453b38cb776d9624ff8e/program/src/processor.rs#L567)).
- **Minting still works:** `MintTo` doesn't check that switch. The issuer can still mint into the account.
  - Test: `credit_switches_belong_to_the_owner_not_the_issuer`
- **Turning confidential credits off makes the account refuse confidential transfers in** ([`valid_as_destination`](https://github.com/solana-program/token-2022/blob/1d91cfca9395a24adcdc453b38cb776d9624ff8e/interface/src/extension/confidential_transfer/mod.rs#L208-L213)) *(code reading)*.

## 8. Other Token-2022 levers (not used in this repo)

- **`PausableConfig`:** a mint-wide pause that blocks confidential deposit, withdraw and transfer ([L422](https://github.com/solana-program/token-2022/blob/1d91cfca9395a24adcdc453b38cb776d9624ff8e/program/src/extension/confidential_transfer/processor.rs#L422), [L554](https://github.com/solana-program/token-2022/blob/1d91cfca9395a24adcdc453b38cb776d9624ff8e/program/src/extension/confidential_transfer/processor.rs#L554), [L651](https://github.com/solana-program/token-2022/blob/1d91cfca9395a24adcdc453b38cb776d9624ff8e/program/src/extension/confidential_transfer/processor.rs#L651)), as well as public transfers. It affects every holder, not one account *(code reading)*.
- **`ConfidentialMintBurn`:** makes the supply itself confidential. Its burn is still owner-signed (see section 2) *(code reading)*.

## 9. Related facts from the other tests

- **Withdraw only reads the available balance.** Proofs built against the pending ciphertext are valid ZK proofs, but `Withdraw` subtracts the amount from the account's *available* ciphertext and compares the result with the proof's remaining-balance ciphertext, so it fails with `ConfidentialTransferBalanceMismatch` ([processor.rs L603-L615](https://github.com/solana-program/token-2022/blob/1d91cfca9395a24adcdc453b38cb776d9624ff8e/program/src/extension/confidential_transfer/processor.rs#L603-L615)). Test: `part2_remit_v2::pending_balance_must_be_applied_before_withdraw`.
- **Only the owner can turn an account confidential.** Anyone can create and fund a user's ATA, but `Reallocate` and `ConfigureAccount` signed by anyone else fail with `OwnerMismatch`. Test: `part2_remit_v2::anyone_can_create_the_ata_but_only_the_owner_can_configure_it`.
- **CPI Guard only blocks the owner's authority.** With `lock_cpi` on, a transfer inside a CPI fails with `CpiGuardTransferBlocked` when the authority is the account owner ([processor.rs L474-L484](https://github.com/solana-program/token-2022/blob/1d91cfca9395a24adcdc453b38cb776d9624ff8e/program/src/processor.rs#L474-L484)). A delegate's transfer through a program still goes through. Test: `extension_delegate_cpi_guard::cpi_guard_blocks_owner_signed_cpi_but_not_the_delegated_path`. The same check does not mention the permanent delegate as authority, so a permanent delegate acting through a CPI on a guarded account is not blocked by it *(code reading)*.
- **CPI Guard can't be toggled from a CPI** (`CpiGuardSettingsLocked`, [cpi_guard/processor.rs L43-L45](https://github.com/solana-program/token-2022/blob/1d91cfca9395a24adcdc453b38cb776d9624ff8e/program/src/extension/cpi_guard/processor.rs#L43-L45)) *(code reading)*.

## Summary table

| Actor | Public `amount` | Pending (encrypted) | Available (encrypted) |
|---|---|---|---|
| Owner | Spend, deposit | Apply | Transfer, withdraw |
| Permanent delegate | Transfer, burn (not while frozen) | Nothing | Nothing |
| Freeze authority | Freeze/thaw the whole account | Freeze blocks apply | Freeze blocks transfer and withdraw |
| Confidential authority | Nothing | Nothing (approval only) | Nothing (approval only) |
| Auditor | Nothing | Nothing | Nothing (reads transfer amounts only) |
