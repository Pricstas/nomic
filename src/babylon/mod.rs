use bitcoin::{
    hashes::Hash,
    psbt::Prevouts,
    secp256k1::{ecdsa, schnorr, PublicKey, Secp256k1},
    util::{
        merkleblock::PartialMerkleTree,
        sighash::SighashCache,
        taproot::{TapLeafHash, TapSighashHash, TaprootBuilder},
    },
    LockTime, OutPoint, Script, Sequence, Transaction, TxIn, TxOut, Txid, Witness, XOnlyPublicKey,
};
use bitcoin_script::bitcoin_script as script;
use cosmos_sdk_proto::traits::TypeUrl;
use ed::{Decode, Encode};
use orga::{
    coins::{Accounts, Address, Amount, Coin, Give, Symbol, Take, Transfer},
    collections::{Deque, Map},
    context::Context,
    encoding::LengthVec,
    macros::Migrate,
    orga,
    plugins::Signer,
    state::State,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    bitcoin::{adapter::Adapter, exempt_from_fee, header_queue::HeaderQueue, Nbtc},
    cosmos::tmhash,
    error::{Error, Result},
};

use crate::bitcoin::threshold_sig::{Pubkey, Signature};

use self::proto::MsgCreateBtcDelegation;

pub mod proto;
pub mod relayer;
pub mod signer;

const MIN_DELEGATION: u64 = 20_000;
const DEFAULT_FP: &str = "bd17e43d349d10f4ff4c1e3591427a8e65197d9a859930def60af21b0ec7b3ce";

/// The symbol for stBTC, a BTC liquid staking token.
#[derive(State, Debug, Clone, Encode, Decode, Default, Migrate, Serialize)]
pub struct Stbtc(());
impl Symbol for Stbtc {
    const INDEX: u8 = 22;
    const NAME: &'static str = "stusat";
}

#[orga]
pub struct Babylon {
    pub delegations: Deque<Delegation>,

    pub(crate) pending_stake: Coin<Nbtc>,
    pub(crate) pending_stake_queue: Deque<(Address, Amount)>,
    pub(crate) pending_stake_by_addr: Map<Address, Amount>,
    pub stake: Accounts<Stbtc>,

    pub(crate) pending_unstake: Coin<Stbtc>,
    pub(crate) pending_unstake_queue: Deque<(Address, Amount)>,
    pub(crate) pending_unstake_by_addr: Map<Address, Amount>,
    pub unstaked: Accounts<Nbtc>,
    // TODO: spendable outputs to renew
    // TODO: spendable outputs (w/ info for ~relay_deposit) to redeposit as nbtc, and dests
    // TODO: config
}

#[orga]
impl Babylon {
    pub fn pending_stake_sats(&self) -> u64 {
        let amount: u64 = self.pending_stake.amount.into();
        amount / 1_000_000 // TODO: get conversion rate from btc config
    }

    pub fn step(&mut self, btc: &crate::bitcoin::Bitcoin) -> Result<Vec<TxOut>> {
        let mut pending_outputs = vec![];

        if self.pending_stake_sats() >= MIN_DELEGATION {
            let del = Delegation::new(
                self.delegations.len(),
                PublicKey::from_slice(
                    btc.checkpoints.active_sigset()?.signatories[0]
                        .pubkey
                        .as_slice(),
                )?
                .into(), // TODO: use frost
                vec![DEFAULT_FP.parse().unwrap()], // TODO: choose dynamically
                1_008,
                101,
                self.pending_stake.take_all()?,
                btc.checkpoints.index,
            )?;
            pending_outputs.push(del.staking_output()?);
            self.delegations.push_back(del)?;

            // TODO: use draining iterator
            // TODO: pay stBTC after delegation is signed
            let mut addrs = vec![];
            for entry in self.pending_stake_by_addr.iter()? {
                let (addr, _) = entry?;
                addrs.push(*addr);
            }
            for addr in addrs {
                self.pending_stake_by_addr.remove(addr)?;
            }

            while let Some(entry) = self.pending_stake_queue.pop_front()? {
                let (addr, amount) = entry.into_inner();
                self.stake.deposit(addr, Stbtc::mint(amount))?;
            }
        }

        //  TODO: process matured delegations, lock in tx which spends the stake and redeposits(?)
        //  TODO: process matured+signed delegations, pay unbonding queue

        Ok(pending_outputs)
    }

    pub fn stake(&mut self, addr: Address, mut coins: Coin<Nbtc>) -> Result<()> {
        let swap_amount = coins.amount.min(self.pending_unstake.amount);
        if swap_amount > 0 {
            let st_coins = self.pay_off_unbond_queue(coins.take(swap_amount)?)?;
            self.stake.deposit(addr, st_coins)?;
        }

        if coins.amount > 0 {
            let prev_amount = self.pending_stake_by_addr.get(addr)?.unwrap_or_default();
            self.pending_stake_by_addr
                .insert(addr, (*prev_amount + coins.amount).result()?)?;
            self.pending_stake_queue.push_back((addr, coins.amount))?;
            self.pending_stake.give(coins)?;
        }

        Ok(())
    }

    fn pay_off_unbond_queue(&mut self, mut coins: Coin<Nbtc>) -> Result<Coin<Stbtc>> {
        if coins.amount > self.pending_unstake.amount {
            return Err(Error::Orga(orga::Error::App(
                "Paying excess into unbond queue".into(),
            )));
        }

        let st_coins = self.pending_unstake.take(coins.amount)?;

        while coins.amount > 0 {
            let (addr, amount) = self
                .pending_unstake_queue
                .pop_front()?
                .ok_or_else(|| Error::Orga(orga::Error::App("No pending unbonds".into())))?
                .into_inner();

            let to_pay = amount.min(coins.amount);
            self.unstaked.deposit(addr, coins.take(to_pay)?)?;
            let prev_amount = self.pending_unstake_by_addr.get(addr)?.unwrap_or_default();
            self.pending_unstake_by_addr
                .insert(addr, (*prev_amount - to_pay).result()?)?;

            let remaining = (amount - to_pay).result()?;
            if remaining > 0 {
                self.pending_unstake_queue.push_front((addr, remaining))?;
            }
        }

        Ok(st_coins)
    }

    fn pay_off_stake_queue(&mut self, mut st_coins: Coin<Stbtc>) -> Result<Coin<Nbtc>> {
        if st_coins.amount > self.pending_stake.amount {
            return Err(Error::Orga(orga::Error::App(
                "Paying excess into stake queue".into(),
            )));
        }

        let coins = self.pending_stake.take(st_coins.amount)?;

        while st_coins.amount > 0 {
            let (addr, amount) = self
                .pending_stake_queue
                .pop_front()?
                .ok_or_else(|| Error::Orga(orga::Error::App("No pending stake".into())))?
                .into_inner();

            let to_pay = amount.min(st_coins.amount);
            self.stake.deposit(addr, st_coins.take(to_pay)?)?;
            let prev_amount = self.pending_stake_by_addr.get(addr)?.unwrap_or_default();
            self.pending_stake_by_addr
                .insert(addr, (*prev_amount - to_pay).result()?)?;

            let remaining = (amount - to_pay).result()?;
            if remaining > 0 {
                self.pending_stake_queue.push_front((addr, remaining))?;
            }
        }

        Ok(coins)
    }

    pub fn unstake(&mut self, addr: Address, amount: Amount) -> Result<()> {
        let mut st_coins = self.stake.withdraw(addr, amount)?;

        let swap_amount = amount.min(self.pending_stake.amount);
        if swap_amount > 0 {
            let coins = self.pay_off_stake_queue(st_coins.take(swap_amount)?)?;
            self.unstaked.deposit(addr, coins)?;
        }

        if st_coins.amount > 0 {
            let prev_amount = self.pending_unstake_by_addr.get(addr)?.unwrap_or_default();
            self.pending_unstake_by_addr
                .insert(addr, (*prev_amount + st_coins.amount).result()?)?;
            self.pending_unstake_queue
                .push_back((addr, st_coins.amount))?;
            self.pending_unstake.give(st_coins)?;
        }

        Ok(())
    }

    // TODO: we shouldn't need this once we can do subclient calls
    #[call]
    pub fn sign(
        &mut self,
        del_index: u64,
        bbn_key: Pubkey,
        btc_sig: Signature,
        bbn_sig: Signature,
        slashing_sig: Signature,
        unbonding_slashing_sig: Signature,
    ) -> Result<()> {
        self.delegations
            .get_mut(del_index)?
            .ok_or_else(|| Error::Orga(orga::Error::App("Delegation not found".to_string())))?
            .sign(
                bbn_key,
                btc_sig,
                bbn_sig,
                slashing_sig,
                unbonding_slashing_sig,
            )
    }
}

pub fn multisig_script(pks: &[XOnlyPublicKey], threshold: u32, verify: bool) -> Result<Script> {
    if pks.is_empty() {
        return Err(Error::Orga(orga::Error::App(
            "No keys provided".to_string(),
        )));
    }

    if threshold > pks.len() as u32 {
        return Err(Error::Orga(orga::Error::App(
            "Required number of valid signers is greater than number of provided keys".to_string(),
        )));
    }

    if pks.len() == 1 {
        return Ok(single_key_script(pks[0], verify));
    }

    let pks = sort_keys(&pks)?;

    let mut bytes = vec![];
    for (i, pk) in pks.iter().enumerate() {
        let pk = pk.serialize().to_vec();
        bytes.extend(
            if i == 0 {
                script! { <pk> OP_CHECKSIG }
            } else {
                script! { <pk> OP_CHECKSIGADD }
            }
            .into_bytes(),
        );
    }
    bytes.extend(script! { <threshold as i64> OP_GREATERTHANOREQUAL }.into_bytes());
    if verify {
        bytes.extend(script! { OP_VERIFY }.into_bytes());
    }

    Ok(bytes.into())
}

pub fn sort_keys(pks: &[XOnlyPublicKey]) -> Result<Vec<XOnlyPublicKey>> {
    if pks.len() < 2 {
        return Err(Error::Orga(orga::Error::App(
            "Cannot sort less than two keys".to_string(),
        )));
    }

    let mut pks = pks.to_vec();
    pks.sort_by(|a, b| a.serialize().cmp(&b.serialize()));

    for i in 0..pks.len() - 1 {
        if pks[i] == pks[i + 1] {
            return Err(Error::Orga(orga::Error::App(
                "Duplicate key in list of keys".to_string(),
            )));
        }
    }

    Ok(pks)
}

pub fn single_key_script(pk: XOnlyPublicKey, verify: bool) -> Script {
    let pk = pk.serialize().to_vec();
    if verify {
        script! { <pk> OP_CHECKSIGVERIFY }
    } else {
        script! { <pk> OP_CHECKSIG }
    }
}

pub fn timelock_script(pk: XOnlyPublicKey, timelock: u64) -> Script {
    let mut bytes = single_key_script(pk, true).into_bytes();
    bytes.extend(script! { <timelock as i64> OP_CSV }.into_bytes());
    bytes.into()
}

pub fn aggregate_scripts(scripts: &[Script]) -> Script {
    let mut bytes = vec![];
    for script in scripts.iter() {
        bytes.extend(script.clone().into_bytes());
    }
    bytes.into()
}

pub struct Params {
    pub covenant_keys: Vec<XOnlyPublicKey>,
    pub covenant_quorum: u32,
    pub slashing_addr: bitcoin::Address,
    pub slashing_min_fee: u64,
}

impl Params {
    pub fn bbn_test_3() -> Self {
        let covenant_keys = [
            "ffeaec52a9b407b355ef6967a7ffc15fd6c3fe07de2844d61550475e7a5233e5",
            "a5c60c2188e833d39d0fa798ab3f69aa12ed3dd2f3bad659effa252782de3c31",
            "59d3532148a597a2d05c0395bf5f7176044b1cd312f37701a9b4d0aad70bc5a4",
            "57349e985e742d5131e1e2b227b5170f6350ac2e2feb72254fcc25b3cee21a18",
            "c8ccb03c379e452f10c81232b41a1ca8b63d0baf8387e57d302c987e5abb8527",
        ];
        let covenant_quorum = 3;

        let slashing_addr = "tb1qv03wm7hxhag6awldvwacy0z42edtt6kwljrhd9";
        let slashing_min_fee = 1_000;

        Self {
            covenant_keys: covenant_keys.iter().map(|k| k.parse().unwrap()).collect(),
            covenant_quorum,
            slashing_addr: slashing_addr.parse().unwrap(),
            slashing_min_fee,
        }
    }
}

pub fn unbonding_script(staker_key: XOnlyPublicKey, params: &Params) -> Result<Script> {
    Ok(aggregate_scripts(&[
        single_key_script(staker_key, true),
        multisig_script(&params.covenant_keys, params.covenant_quorum, false)?,
    ]))
}

pub fn slashing_script(
    staker_key: XOnlyPublicKey,
    fp_keys: &[XOnlyPublicKey],
    params: &Params,
) -> Result<Script> {
    Ok(aggregate_scripts(&[
        single_key_script(staker_key, true),
        multisig_script(fp_keys, 1, true)?,
        multisig_script(&params.covenant_keys, params.covenant_quorum, false)?,
    ]))
}

const UNSPENDABLE_KEY: &str = "50929b74c1a04954b78b4b6035e97a5e078a5a0f28ec96d547bfee9ace803ac0";

pub fn staking_script(
    staker_key: XOnlyPublicKey,
    fp_keys: &[XOnlyPublicKey],
    staking_time: u16,
    params: &Params,
) -> Result<Script> {
    let timelock_script = timelock_script(staker_key, staking_time as u64);
    let unbonding_script = unbonding_script(staker_key, params)?;
    let slashing_script = slashing_script(staker_key, fp_keys, params)?;

    let internal_key = UNSPENDABLE_KEY.parse()?;
    let spend_info = TaprootBuilder::new()
        .add_leaf(2, timelock_script)?
        .add_leaf(2, unbonding_script)?
        .add_leaf(1, slashing_script)?
        .finalize(&Secp256k1::new(), internal_key)
        .map_err(|_| Error::Orga(orga::Error::App("Failed to finalize taproot".to_string())))?;

    Ok(Script::new_v1_p2tr_tweaked(spend_info.output_key()))
}

pub fn slashing_tx(
    staker_key: XOnlyPublicKey,
    stake_out: OutPoint,
    stake_value: u64,
    unbonding_time: u16,
    params: &Params,
) -> Result<Transaction> {
    let staking_in = TxIn {
        previous_output: stake_out,
        script_sig: Script::new(),
        sequence: Sequence(u32::MAX),
        witness: Witness::default(),
    };

    let slashing_rate = (9, 10); // TODO: parameterize
    let slashing_value = stake_value * slashing_rate.0 / slashing_rate.1;
    let slashing_out = TxOut {
        value: slashing_value,
        script_pubkey: params.slashing_addr.script_pubkey(),
    };

    let change_key = TaprootBuilder::new()
        .add_leaf(0, timelock_script(staker_key, unbonding_time as u64))?
        .finalize(&Secp256k1::new(), UNSPENDABLE_KEY.parse().unwrap())
        .unwrap()
        .output_key();
    let change_out = TxOut {
        value: stake_value - slashing_value - params.slashing_min_fee,
        script_pubkey: Script::new_v1_p2tr_tweaked(change_key),
    };

    Ok(Transaction {
        version: 2,
        lock_time: bitcoin::PackedLockTime(0),
        input: vec![staking_in],
        output: vec![slashing_out, change_out],
    })
}

pub fn unbonding_tx(
    staker_key: XOnlyPublicKey,
    fp_keys: &[XOnlyPublicKey],
    staking_outpoint: OutPoint,
    staking_value: u64,
    unbonding_time: u16,
    params: &Params,
) -> Result<Transaction> {
    let staking_in = TxIn {
        previous_output: staking_outpoint,
        script_sig: Script::new(),
        sequence: Sequence(u32::MAX),
        witness: Witness::default(),
    };

    let out_key = TaprootBuilder::new()
        .add_leaf(1, timelock_script(staker_key, unbonding_time as u64))?
        .add_leaf(1, slashing_script(staker_key, fp_keys, params)?)?
        .finalize(&Secp256k1::new(), UNSPENDABLE_KEY.parse().unwrap())
        .unwrap()
        .output_key();
    let out = TxOut {
        value: staking_value - 1_000, // TODO: parameterize
        script_pubkey: Script::new_v1_p2tr_tweaked(out_key),
    };

    Ok(Transaction {
        version: 2,
        lock_time: bitcoin::PackedLockTime(0),
        input: vec![staking_in],
        output: vec![out],
    })
}

pub type XOnlyPubkey = [u8; 32];

fn bytes_to_pubkey(bytes: XOnlyPubkey) -> Result<XOnlyPublicKey> {
    Ok(XOnlyPublicKey::from_slice(&bytes)?)
}

#[derive(PartialEq, Eq, Clone, Copy)]
pub enum DelegationStatus {
    Created,
    Signing,
    Signed,
}

#[orga]
#[derive(Debug)]
pub struct Delegation {
    pub index: u64,
    pub btc_key: XOnlyPubkey,
    pub fp_keys: LengthVec<u8, XOnlyPubkey>,
    pub staking_period: u16,
    pub unbonding_period: u16,
    pub stake: Coin<Nbtc>,
    pub checkpoint_index: u32,

    pub staking_outpoint: Option<crate::bitcoin::adapter::Adapter<OutPoint>>,
    pub staking_height: Option<u32>,

    // TODO: don't use basic pubkey, use Interchain Accounts
    pub bbn_key: Option<Pubkey>,
    pub(crate) pop_btc_sig: Option<Signature>,
    pub(crate) pop_bbn_sig: Option<Signature>,
    pub(crate) slashing_tx_sig: Option<Signature>,
    pub(crate) unbonding_slashing_tx_sig: Option<Signature>,
    // TODO: tx to spend mature stake
}

#[orga]
impl Delegation {
    pub fn new(
        index: u64,
        btc_key: XOnlyPublicKey,
        fp_keys: Vec<XOnlyPublicKey>,
        staking_period: u16,
        unbonding_period: u16,
        stake: Coin<Nbtc>,
        checkpoint_index: u32,
    ) -> Result<Self> {
        Ok(Self {
            index,
            btc_key: btc_key.serialize(),
            fp_keys: fp_keys
                .iter()
                .map(|k| k.serialize())
                .collect::<Vec<_>>()
                .try_into()?,
            staking_period,
            unbonding_period,
            stake,
            checkpoint_index,
            ..Default::default()
        })
    }

    // TODO: remove conversion methods once orga can transparently convert

    fn btc_key(&self) -> Result<XOnlyPublicKey> {
        bytes_to_pubkey(self.btc_key)
    }

    fn fp_keys(&self) -> Result<Vec<XOnlyPublicKey>> {
        self.fp_keys.iter().cloned().map(bytes_to_pubkey).collect()
    }

    pub fn stake_sats(&self) -> u64 {
        let stake_amount: u64 = self.stake.amount.into();
        stake_amount / 1_000_000 // TODO: get covnersion from bitcoin config
    }

    #[call]
    pub fn sign(
        &mut self,
        bbn_key: Pubkey,
        btc_sig: Signature,
        bbn_sig: Signature,
        slashing_sig: Signature,
        unbonding_slashing_sig: Signature,
    ) -> Result<()> {
        exempt_from_fee()?;

        let status = self.status()?;
        if status != DelegationStatus::Signing {
            let msg = match status {
                DelegationStatus::Created => "Not ready to sign",
                DelegationStatus::Signing => unreachable!(),
                DelegationStatus::Signed => "Already signed",
            };
            return Err(Error::Orga(orga::Error::App(msg.to_string())));
        }

        verify_pop(self.btc_key()?, bbn_key, btc_sig, bbn_sig)?;
        self.bbn_key = Some(bbn_key);
        self.pop_btc_sig = Some(btc_sig);
        self.pop_bbn_sig = Some(bbn_sig);

        // TODO: reuse secp instance
        let secp = Secp256k1::verification_only();
        let key = bytes_to_pubkey(self.btc_key)?;

        let slashing_sighash = self.slashing_sighash()?;
        let msg = bitcoin::secp256k1::Message::from_slice(&slashing_sighash.into_inner())?;
        let sig = schnorr::Signature::from_slice(slashing_sig.as_slice())?;
        #[cfg(not(fuzzing))]
        secp.verify_schnorr(&sig, &msg, &key)?;
        self.slashing_tx_sig = Some(slashing_sig);

        let unbonding_slashing_sighash = self.unbonding_slashing_sighash()?;
        let msg =
            bitcoin::secp256k1::Message::from_slice(&unbonding_slashing_sighash.into_inner())?;
        let sig = schnorr::Signature::from_slice(unbonding_slashing_sig.as_slice())?;
        #[cfg(not(fuzzing))]
        secp.verify_schnorr(&sig, &msg, &key)?;
        self.unbonding_slashing_tx_sig = Some(unbonding_slashing_sig);

        Ok(())
    }

    pub fn relay_staking_tx(
        &mut self,
        headers: &HeaderQueue,
        height: u32,
        proof: PartialMerkleTree,
        tx: Transaction,
        vout: u32,
    ) -> Result<()> {
        if self.status()? != DelegationStatus::Created {
            return Err(Error::Orga(orga::Error::App(
                "Staking tx already relayed".to_string(),
            )));
        }

        // TODO: move to config
        if headers.height()?.saturating_sub(height) < 1 {
            return Err(Error::Orga(orga::Error::App(
                "Staking tx is not confirmed".to_string(),
            )));
        }

        // TODO: dedupe this with other proof verification calls
        let header = headers
            .get_by_height(height)?
            .ok_or_else(|| Error::Orga(orga::Error::App("Header not found".to_string())))?;
        let mut txids = vec![];
        let mut block_indexes = vec![];
        let proof_merkle_root = proof
            .extract_matches(&mut txids, &mut block_indexes)
            .map_err(|_| Error::BitcoinMerkleBlockError)?;
        if proof_merkle_root != header.merkle_root() {
            return Err(orga::Error::App(
                "Bitcoin merkle proof does not match header".to_string(),
            ))?;
        }
        if txids.len() != 1 {
            return Err(orga::Error::App(
                "Bitcoin merkle proof contains an invalid number of txids".to_string(),
            ))?;
        }
        if txids[0] != tx.txid() {
            return Err(orga::Error::App(
                "Bitcoin merkle proof does not match transaction".to_string(),
            ))?;
        }

        if vout as usize >= tx.output.len() {
            return Err(orga::Error::App(
                "Output index is out of bounds".to_string(),
            ))?;
        }
        let output = &tx.output[vout as usize];

        if output.value != self.stake_sats() {
            // TODO: get conversion from config
            return Err(orga::Error::App(
                "Staking amount does not match".to_string(),
            ))?;
        }
        if output.script_pubkey != self.staking_script()? {
            return Err(orga::Error::App(
                "Staking script pubkey does not match".to_string(),
            ))?;
        }

        let outpoint = OutPoint {
            txid: tx.txid(),
            vout,
        };
        self.staking_outpoint = Some(outpoint.into());
        self.staking_height = Some(height);

        Ok(())
    }

    pub fn status(&self) -> Result<DelegationStatus> {
        assert_eq!(self.bbn_key.is_none(), self.pop_btc_sig.is_none());
        assert_eq!(self.bbn_key.is_none(), self.pop_bbn_sig.is_none());
        assert_eq!(self.bbn_key.is_none(), self.slashing_tx_sig.is_none());
        assert_eq!(
            self.bbn_key.is_none(),
            self.unbonding_slashing_tx_sig.is_none()
        );
        assert_eq!(
            self.staking_outpoint.is_none(),
            self.staking_height.is_none()
        );

        if self.staking_outpoint.is_none() {
            return Ok(DelegationStatus::Created);
        }
        if self.bbn_key.is_none() {
            return Ok(DelegationStatus::Signing);
        }
        Ok(DelegationStatus::Signed)
    }

    pub fn staking_output(&self) -> Result<TxOut> {
        Ok(TxOut {
            value: self.stake_sats(),
            script_pubkey: self.staking_script()?,
        })
    }

    pub fn staking_script(&self) -> Result<Script> {
        Ok(staking_script(
            self.btc_key()?,
            &self.fp_keys()?,
            self.staking_period,
            &Params::bbn_test_3(),
        )?)
    }

    pub fn unbonding_tx(&self) -> Result<Transaction> {
        Ok(unbonding_tx(
            self.btc_key()?,
            &self.fp_keys()?,
            *self.staking_outpoint.ok_or_else(|| {
                Error::Orga(orga::Error::App("Missing staking outpoint".to_string()))
            })?,
            self.stake_sats(),
            self.unbonding_period,
            &Params::bbn_test_3(),
        )?)
    }

    pub fn slashing_tx(&self) -> Result<Transaction> {
        Ok(slashing_tx(
            self.btc_key()?,
            *self.staking_outpoint.ok_or_else(|| {
                Error::Orga(orga::Error::App("Missing staking outpoint".to_string()))
            })?,
            self.stake_sats(),
            self.unbonding_period,
            &Params::bbn_test_3(),
        )?)
    }

    pub fn unbonding_slashing_tx(&self) -> Result<Transaction> {
        Ok(slashing_tx(
            self.btc_key()?,
            OutPoint {
                txid: self.unbonding_tx()?.txid(),
                vout: 0,
            },
            self.unbonding_tx()?.output[0].value,
            self.unbonding_period,
            &Params::bbn_test_3(),
        )?)
    }

    pub fn slashing_sighash(&self) -> Result<TapSighashHash> {
        let slashing_tx = self.slashing_tx()?;
        let mut sc = SighashCache::new(&slashing_tx);
        Ok(sc.taproot_script_spend_signature_hash(
            0,
            &Prevouts::All(&[&TxOut {
                script_pubkey: self.staking_script()?,
                value: self.stake_sats(),
            }]),
            TapLeafHash::from_script(
                &slashing_script(self.btc_key()?, &self.fp_keys()?, &Params::bbn_test_3())?,
                bitcoin::util::taproot::LeafVersion::TapScript,
            ),
            bitcoin::SchnorrSighashType::Default,
        )?)
    }

    pub fn unbonding_slashing_sighash(&self) -> Result<TapSighashHash> {
        let unbonding_tx = self.unbonding_tx()?;
        let slashing_tx = self.unbonding_slashing_tx()?;
        let mut sc = SighashCache::new(&slashing_tx);
        Ok(sc.taproot_script_spend_signature_hash(
            0,
            &Prevouts::All(&[&unbonding_tx.output[0]]),
            TapLeafHash::from_script(
                &slashing_script(self.btc_key()?, &self.fp_keys()?, &Params::bbn_test_3())?,
                bitcoin::util::taproot::LeafVersion::TapScript,
            ),
            bitcoin::SchnorrSighashType::Default,
        )?)
    }
}

pub fn verify_pop(
    btc_key: XOnlyPublicKey,
    bbn_key: Pubkey,
    btc_sig: Signature,
    bbn_sig: Signature,
) -> Result<()> {
    // TODO: reuse secp instance
    let secp = Secp256k1::verification_only();

    let bbn_key = PublicKey::from_slice(&bbn_key.as_slice())?;
    let bbn_msg = bitcoin::secp256k1::Message::from_slice(&btc_key.serialize())?;
    let bbn_sig_hex = hex::encode(bbn_sig.as_slice());
    let bbn_sig = ecdsa::Signature::from_compact(bbn_sig.as_slice())?;
    #[cfg(not(fuzzing))]
    secp.verify_ecdsa(&bbn_msg, &bbn_sig, &bbn_key)?;

    let mut hasher = Sha256::new();
    hasher.update(&bbn_sig_hex.as_bytes());
    let hash = hasher.finalize().to_vec();
    let btc_msg = bitcoin::secp256k1::Message::from_slice(&hash)?;
    let btc_sig = schnorr::Signature::from_slice(btc_sig.as_slice())?;
    #[cfg(not(fuzzing))]
    secp.verify_schnorr(&btc_sig, &btc_msg, &btc_key)?;

    Ok(())
}

impl TypeUrl for MsgCreateBtcDelegation {
    const TYPE_URL: &'static str = "/babylon.btcstaking.v1.MsgCreateBTCDelegation";
}

fn tree_hash(left: Option<[u8; 32]>, right: Option<[u8; 32]>) -> Option<[u8; 32]> {
    if left.is_none() && right.is_none() {
        return None;
    }

    let mut first = Sha256::new();
    first.update(left.unwrap());
    first.update(right.unwrap_or(left.unwrap()));

    let mut second = Sha256::new();
    second.update(first.finalize());
    Some(second.finalize().into())
}

fn tree_node(hashes: &[[u8; 32]], index: u32, level: u32) -> Option<[u8; 32]> {
    if level == 0 {
        return hashes.get(index as usize).map(|h| *h);
    }

    let left = tree_node(hashes, index * 2, level - 1)?;
    let right = tree_node(hashes, index * 2 + 1, level - 1);
    tree_hash(Some(left), right)
}

pub fn create_proof(txids: &[Txid], target_txid: Txid) -> (u32, Vec<u8>) {
    let index = txids.iter().position(|txid| *txid == target_txid).unwrap() as u32;
    let hashes: Vec<_> = txids.iter().map(|txid| txid.into_inner()).collect();

    let mut proof_bytes = vec![];
    let mut level = 0;
    let mut idx = index;
    loop {
        let sibling = tree_node(&hashes, idx ^ 1, level);
        if sibling.is_none() {
            break;
        }
        proof_bytes.extend_from_slice(&sibling.unwrap());
        level += 1;
        idx >>= 1;
    }

    (index, proof_bytes)
}

#[cfg(test)]
mod tests {
    use bitcoin::{psbt::serialize::Deserialize, Network};

    use super::*;

    #[test]
    fn staking_output_fixture() {
        let staker_btc_pk = "b3193611fc3fad7c35847dc98fb3bbc22f7c86fa87a5b5d3c64e06bf4e2ff54b";
        let fp_pk = "14102e9fedd4a93e0955c07ba06a598309e75371b7bb8645717abb37b5fde939";
        let staking_time = 1_008;
        let expected_staking_addr =
            "tb1p9e7vhkuskwfzyt8wz4v2769p9wd0et3gz78y39hawpm2ekeqjawqakm862";

        let staking_script = staking_script(
            staker_btc_pk.parse().unwrap(),
            &[fp_pk.parse().unwrap()],
            staking_time,
            &Params::bbn_test_3(),
        )
        .unwrap();
        let staking_addr = bitcoin::Address::from_script(&staking_script, Network::Signet).unwrap();
        assert_eq!(staking_addr.to_string(), expected_staking_addr);
    }

    #[test]
    fn slashing_tx_fixture() {
        let staker_btc_pk = "b3193611fc3fad7c35847dc98fb3bbc22f7c86fa87a5b5d3c64e06bf4e2ff54b";
        let staking_outpoint = OutPoint {
            txid: "56f6d24069d3d8ef40f6dc7363d4acc1fde502610ad80ee3476aa5b8e8ad7a23"
                .parse()
                .unwrap(),
            vout: 0,
        };
        let staking_value = 20_000;
        let unbonding_time = 101;
        let expected_slashing_tx = Transaction::deserialize(&hex::decode("0200000001237aade8b8a56a47e30ed80a6102e5fdc1acd46373dcf640efd8d36940d2f6560000000000ffffffff02d00700000000000016001463e2edfae6bf51aebbed63bb823c55565ab5eace6842000000000000225120e9f60075bdb745bb352fee26ee981fd55573652a928c8e6b19db29e00f32646000000000").unwrap()).unwrap();

        let slashing_tx = slashing_tx(
            staker_btc_pk.parse().unwrap(),
            staking_outpoint,
            staking_value,
            unbonding_time,
            &Params::bbn_test_3(),
        )
        .unwrap();
        assert_eq!(slashing_tx, expected_slashing_tx);
    }

    #[test]
    fn unbonding_tx_fixture() {
        let staker_btc_pk = "b3193611fc3fad7c35847dc98fb3bbc22f7c86fa87a5b5d3c64e06bf4e2ff54b";
        let fp_pk = "14102e9fedd4a93e0955c07ba06a598309e75371b7bb8645717abb37b5fde939";
        let staking_outpoint = OutPoint {
            txid: "56f6d24069d3d8ef40f6dc7363d4acc1fde502610ad80ee3476aa5b8e8ad7a23"
                .parse()
                .unwrap(),
            vout: 0,
        };
        let staking_value = 20_000;
        let unbonding_time = 101;
        let expected_unbonding_tx = Transaction::deserialize(&hex::decode("0200000001237aade8b8a56a47e30ed80a6102e5fdc1acd46373dcf640efd8d36940d2f6560000000000ffffffff01384a00000000000022512019d293a8610a102e8d62c1ed6c054b759e4361d5352e268ad0c39582734ea50b00000000").unwrap()).unwrap();
        let expected_unbonding_slashing_tx = Transaction::deserialize(&hex::decode("02000000017285541c7b224b952f32eb16a815db378fc338502edcd64e09cf0eb9417f5deb0000000000ffffffff026c0700000000000016001463e2edfae6bf51aebbed63bb823c55565ab5eacee43e000000000000225120e9f60075bdb745bb352fee26ee981fd55573652a928c8e6b19db29e00f32646000000000").unwrap()).unwrap();

        let unbonding_tx = unbonding_tx(
            staker_btc_pk.parse().unwrap(),
            &[fp_pk.parse().unwrap()],
            staking_outpoint,
            staking_value,
            unbonding_time,
            &Params::bbn_test_3(),
        )
        .unwrap();
        assert_eq!(unbonding_tx, expected_unbonding_tx);

        let unbonding_slashing_tx = slashing_tx(
            staker_btc_pk.parse().unwrap(),
            OutPoint {
                txid: unbonding_tx.txid(),
                vout: 0,
            },
            unbonding_tx.output[0].value,
            unbonding_time,
            &Params::bbn_test_3(),
        )
        .unwrap();
        assert_eq!(unbonding_slashing_tx, expected_unbonding_slashing_tx);
    }

    #[test]
    fn pop_fixture() {
        let btc_key = "";
        let bbn_key = "";
        let bbn_sig = "";
        let btc_sig = "";

        // TODO
    }
}
