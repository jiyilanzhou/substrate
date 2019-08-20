// Copyright 2019 Parity Technologies (UK) Ltd.
// This file is part of Substrate.

// Substrate is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Substrate is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Substrate.  If not, see <http://www.gnu.org/licenses/>.

//! Consensus extension module for BABE consensus. Collects on-chain randomness
//! from VRF outputs and manages epoch transitions.

#![cfg_attr(not(feature = "std"), no_std)]
#![forbid(unused_must_use, unsafe_code, unused_variables)]

#![forbid(dead_code)]

pub use timestamp;

use rstd::{result, prelude::*};
use srml_support::{
	decl_storage, decl_module, StorageValue, StorageMap, Parameter,
	traits::{FindAuthor, Get, KeyOwnerProofSystem},
};
use timestamp::{OnTimestampSet};
use sr_primitives::{
	generic::{DigestItem, Era, UncheckedExtrinsic}, ConsensusEngineId, Perbill,
	traits::{IsMember, SaturatedConversion, Saturating, RandomnessBeacon, Header, Zero},
	weights::SimpleDispatchInfo, EquivocationProof, key_types, KeyTypeId
};
use sr_staking_primitives::{
	SessionIndex,
	offence::{ReportOffence, Offence, Kind},
};
#[cfg(feature = "std")]
use timestamp::TimestampInherentData;
use codec::{Encode, Decode};
use inherents::{RuntimeString, InherentIdentifier, InherentData, ProvideInherent, MakeFatalError};
#[cfg(feature = "std")]
use inherents::{InherentDataProviders, ProvideInherentData};
use system::{ensure_signed, ensure_root};
use babe_primitives::{
	BABE_ENGINE_ID, ConsensusLog, BabeAuthorityWeight, Epoch, RawBabePreDigest, get_slot
};
use app_crypto::RuntimeAppPublic;
pub use babe_primitives::{AuthorityId, AuthoritySignature, VRF_OUTPUT_LENGTH, PUBLIC_KEY_LENGTH, app};

/// The BABE inherent identifier.
pub const INHERENT_IDENTIFIER: InherentIdentifier = *b"babeslot";

/// The type of the BABE inherent.
pub type InherentType = u64;
/// Auxiliary trait to extract BABE inherent data.
pub trait BabeInherentData {
	/// Get BABE inherent data.
	fn babe_inherent_data(&self) -> result::Result<InherentType, RuntimeString>;
	/// Replace BABE inherent data.
	fn babe_replace_inherent_data(&mut self, new: InherentType);
}

impl BabeInherentData for InherentData {
	fn babe_inherent_data(&self) -> result::Result<InherentType, RuntimeString> {
		self.get_data(&INHERENT_IDENTIFIER)
			.and_then(|r| r.ok_or_else(|| "BABE inherent data not found".into()))
	}

	fn babe_replace_inherent_data(&mut self, new: InherentType) {
		self.replace_data(INHERENT_IDENTIFIER, &new);
	}
}

/// Provides the slot duration inherent data for BABE.
#[cfg(feature = "std")]
pub struct InherentDataProvider {
	slot_duration: u64,
}

#[cfg(feature = "std")]
impl InherentDataProvider {
	/// Constructs `Self`
	pub fn new(slot_duration: u64) -> Self {
		Self {
			slot_duration
		}
	}
}

#[cfg(feature = "std")]
impl ProvideInherentData for InherentDataProvider {
	fn on_register(
		&self,
		providers: &InherentDataProviders,
	) -> result::Result<(), RuntimeString> {
		if !providers.has_provider(&timestamp::INHERENT_IDENTIFIER) {
			// Add the timestamp inherent data provider, as we require it.
			providers.register_provider(timestamp::InherentDataProvider)
		} else {
			Ok(())
		}
	}

	fn inherent_identifier(&self) -> &'static inherents::InherentIdentifier {
		&INHERENT_IDENTIFIER
	}

	fn provide_inherent_data(
		&self,
		inherent_data: &mut InherentData,
	) -> result::Result<(), RuntimeString> {
		let timestamp = inherent_data.timestamp_inherent_data()?;
		let slot_number = timestamp / self.slot_duration;
		inherent_data.put_data(INHERENT_IDENTIFIER, &slot_number)
	}

	fn error_to_string(&self, error: &[u8]) -> Option<String> {
		RuntimeString::decode(&mut &error[..]).map(Into::into).ok()
	}
}

pub trait Trait: timestamp::Trait + balances::Trait + indices::Trait + Send + Sync {
	type EpochDuration: Get<u64>;
	type ExpectedBlockTime: Get<Self::Moment>;
	type IdentificationTuple: Parameter;
	type Proof: Parameter;

	type KeyOwnerSystem: KeyOwnerProofSystem<
		(KeyTypeId, Vec<u8>),
		Proof=Self::Proof,
		IdentificationTuple=Self::IdentificationTuple,
	>;
	type ReportEquivocation: ReportOffence<
		Self::AccountId,
		Self::IdentificationTuple,
		BabeEquivocationOffence<Self::IdentificationTuple>,
	>;
}

/// The length of the BABE randomness
pub const RANDOMNESS_LENGTH: usize = 32;

const UNDER_CONSTRUCTION_SEGMENT_LENGTH: usize = 256;

decl_storage! {
	trait Store for Module<T: Trait> as Babe {
		/// Current epoch index.
		pub EpochIndex get(epoch_index): u64;

		/// Current epoch authorities.
		pub Authorities get(authorities): Vec<(AuthorityId, BabeAuthorityWeight)>;

		/// Slot at which the current epoch started. It is possible that no
		/// block was authored at the given slot and the epoch change was
		/// signalled later than this.
		pub EpochStartSlot get(epoch_start_slot): u64;

		/// Current slot number.
		pub CurrentSlot get(current_slot): u64;

		/// Whether secondary slots are enabled in case the VRF-based slot is
		/// empty for the current epoch and the next epoch, respectively.
		pub SecondarySlots get(secondary_slots): (bool, bool) = (true, true);

		/// Pending change to enable/disable secondary slots which will be
		/// triggered at `current_epoch + 2`.
		pub PendingSecondarySlotsChange get(pending_secondary_slots_change): Option<bool> = None;

		/// The epoch randomness for the *current* epoch.
		///
		/// # Security
		///
		/// This MUST NOT be used for gambling, as it can be influenced by a
		/// malicious validator in the short term. It MAY be used in many
		/// cryptographic protocols, however, so long as one remembers that this
		/// (like everything else on-chain) it is public. For example, it can be
		/// used where a number is needed that cannot have been chosen by an
		/// adversary, for purposes such as public-coin zero-knowledge proofs.
		// NOTE: the following fields don't use the constants to define the
		// array size because the metadata API currently doesn't resolve the
		// variable to its underlying value.
		pub Randomness get(randomness): [u8; 32 /* RANDOMNESS_LENGTH */];

		/// Next epoch randomness.
		NextRandomness: [u8; 32 /* RANDOMNESS_LENGTH */];

		/// Randomness under construction.
		///
		/// We make a tradeoff between storage accesses and list length.
		/// We store the under-construction randomness in segments of up to
		/// `UNDER_CONSTRUCTION_SEGMENT_LENGTH`.
		///
		/// Once a segment reaches this length, we begin the next one.
		/// We reset all segments and return to `0` at the beginning of every
		/// epoch.
		SegmentIndex build(|_| 0): u32;
		UnderConstruction: map u32 => Vec<[u8; 32 /* VRF_OUTPUT_LENGTH */]>;

		/// Temporary value (cleared at block finalization) which is true
		/// if per-block initialization has already been called for current block.
		Initialized get(initialized): Option<bool>;
	}
	add_extra_genesis {
		config(authorities): Vec<(AuthorityId, BabeAuthorityWeight)>;
		build(|
			storage: &mut (sr_primitives::StorageOverlay, sr_primitives::ChildrenStorageOverlay),
			config: &GenesisConfig
		| {
			sr_io::with_storage(
				storage,
				|| Module::<T>::initialize_authorities(&config.authorities),
			);
		})
	}
}

fn equivocation_is_valid<T: Trait>(
	equivocation: &EquivocationProof<T::Header, AuthorityId, AuthoritySignature>,
) -> bool {
	let first_header = &equivocation.first_header;
	let second_header = &equivocation.second_header;

	if first_header == second_header {
		return false
	}

	let maybe_first_slot = get_slot::<T::Header>(&first_header);
	let maybe_second_slot = get_slot::<T::Header>(&second_header);

	if maybe_first_slot.is_err() || maybe_second_slot.is_err() {
		return false
	}
	let first_slot = maybe_first_slot.expect("checked before; qed");
	let second_slot = maybe_second_slot.expect("checked before; qed");

	if equivocation.slot == first_slot && first_slot == second_slot {
		let author = &equivocation.identity;

		if !author.verify(&first_header.hash(), &equivocation.first_signature) {
			return false
		}
		if !author.verify(&second_header.hash(), &equivocation.second_signature) {
			return false
		}
		return true;
	}

	false
}

decl_module! {
	/// The BABE SRML module
	pub struct Module<T: Trait> for enum Call where origin: T::Origin {
		/// The number of **slots** that an epoch takes. We couple sessions to
		/// epochs, i.e. we start a new session once the new epoch begins.
		const EpochDuration: u64 = T::EpochDuration::get();

		/// The expected average block time at which BABE should be creating
		/// blocks. Since BABE is probabilistic it is not trivial to figure out
		/// what the expected average block time should be based on the slot
		/// duration and the security parameter `c` (where `1 - c` represents
		/// the probability of a slot being empty).
		const ExpectedBlockTime: T::Moment = T::ExpectedBlockTime::get();

		/// Initialization
		fn on_initialize() {
			Self::do_initialize();
		}

		/// Block finalization
		fn on_finalize() {
			Initialized::kill();
		}

		/// Report a Babe equivocation.
		fn report_equivocation(
			origin,
			equivocation: EquivocationProof<T::Header, AuthorityId, AuthoritySignature>,
			proof: T::Proof
		) {
			let who = ensure_signed(origin)?;

			if !equivocation_is_valid::<T>(&equivocation) {
				return Err("invalid equivocation")
			}

			let to_punish = <T as Trait>::KeyOwnerSystem::check_proof(
				(key_types::BABE, equivocation.identity.encode()),
				proof.clone(),
			);
			if let Some(to_punish) = to_punish {
				let offence = BabeEquivocationOffence {
					slot: equivocation.slot,
					session_index: SessionIndex::default(),
					validator_set_count: 0,
					offender: to_punish,
				};
				T::ReportEquivocation::report_offence(vec![who], offence);
			}
		}

		/// Sets a pending change to enable / disable secondary slot assignment.
		/// The pending change will be set at the end of the current epoch and
		/// will be enacted at `current_epoch + 2`.
		#[weight = SimpleDispatchInfo::FixedOperational(10_000)]
		fn set_pending_secondary_slots_change(origin, change: Option<bool>) {
			ensure_root(origin)?;
			match change {
				Some(change) =>	PendingSecondarySlotsChange::put(change),
				None => {
					PendingSecondarySlotsChange::take();
				},
			}
		}
	}
}

impl<T: Trait> RandomnessBeacon for Module<T> {
	fn random() -> [u8; VRF_OUTPUT_LENGTH] {
		Self::randomness()
	}
}

/// A BABE public key
pub type BabeKey = [u8; PUBLIC_KEY_LENGTH];

impl<T: Trait> FindAuthor<u32> for Module<T> {
	fn find_author<'a, I>(digests: I) -> Option<u32> where
		I: 'a + IntoIterator<Item=(ConsensusEngineId, &'a [u8])>
	{
		for (id, mut data) in digests.into_iter() {
			if id == BABE_ENGINE_ID {
				let pre_digest = RawBabePreDigest::decode(&mut data).ok()?;
				return Some(match pre_digest {
					RawBabePreDigest::Primary { authority_index, .. } =>
						authority_index,
					RawBabePreDigest::Secondary { authority_index, .. } =>
						authority_index,
				});
			}
		}

		return None;
	}
}

impl<T: Trait> IsMember<AuthorityId> for Module<T> {
	fn is_member(authority_id: &AuthorityId) -> bool {
		<Module<T>>::authorities()
			.iter()
			.any(|id| &id.0 == authority_id)
	}
}

impl<T: Trait> session::ShouldEndSession<T::BlockNumber> for Module<T> {
	fn should_end_session(_: T::BlockNumber) -> bool {
		// it might be (and it is in current implementation) that session module is calling
		// should_end_session() from it's own on_initialize() handler
		// => because session on_initialize() is called earlier than ours, let's ensure
		// that we have synced with digest before checking if session should be ended
		Self::do_initialize();

		let diff = CurrentSlot::get().saturating_sub(EpochStartSlot::get());
		diff >= T::EpochDuration::get()
	}
}

/// A BABE equivocation offence report.
///
/// When a validator released two or more blocks at the same slot.
pub struct BabeEquivocationOffence<FullIdentification> {
	/// A babe slot number in which this incident happened.
	slot: u64,
	/// The session index in which the incident happened.
	session_index: SessionIndex,
	/// The size of the validator set at the time of the offence.
	validator_set_count: u32,
	/// The authority that produced the equivocation.
	offender: FullIdentification,
}

impl<FullIdentification: Clone> Offence<FullIdentification> for BabeEquivocationOffence<FullIdentification> {
	const ID: Kind = *b"babe:equivocatio";
	type TimeSlot = u64;

	fn offenders(&self) -> Vec<FullIdentification> {
		vec![self.offender.clone()]
	}

	fn session_index(&self) -> SessionIndex {
		self.session_index
	}

	fn validator_set_count(&self) -> u32 {
		self.validator_set_count
	}

	fn time_slot(&self) -> Self::TimeSlot {
		self.slot
	}

	fn slash_fraction(
		offenders_count: u32,
		validator_set_count: u32,
	) -> Perbill {
		// the formula is min((3k / n)^2, 1)
		let x = Perbill::from_rational_approximation(3 * offenders_count, validator_set_count);
		// _ ^ 2
		x.square()
	}
}

impl<T: Trait> Module<T> {
	/// Determine the BABE slot duration based on the Timestamp module configuration.
	pub fn slot_duration() -> T::Moment {
		// we double the minimum block-period so each author can always propose within
		// the majority of their slot.
		<T as timestamp::Trait>::MinimumPeriod::get().saturating_mul(2.into())
	}

	pub fn construct_equivocation_transaction(
		equivocation: EquivocationProof<T::Header, AuthorityId, AuthoritySignature>
	) -> Option<Vec<u8>> {
		let proof = T::KeyOwnerSystem::prove((
			key_types::BABE,
			equivocation.identity.encode(),
		))?;

		let local_keys = app::Public::all();

		if local_keys.len() > 0 {
			let reporter = &local_keys[0];
			let function = Call::report_equivocation::<T>(equivocation, proof);

			// TODO: fix these parameters.
			let check_genesis = system::CheckGenesis::<T>::new();
			let check_era = system::CheckEra::<T>::from(Era::Immortal);
			let check_nonce = system::CheckNonce::<T>::from(Default::default());
			let check_weight = system::CheckWeight::<T>::new();
			let take_fees = balances::TakeFees::<T>::from(Default::default());
			let extra = (check_genesis, check_era, check_nonce, check_weight, take_fees);

			let genesis_hash = <system::Module<T>>::block_hash(T::BlockNumber::zero());
			let raw_payload = (function, extra.clone(), genesis_hash, genesis_hash);

			let maybe_signature: Option<app::Signature> = raw_payload.using_encoded(|payload| if payload.len() > 256 {
				reporter.sign(&sr_io::blake2_256(payload))
			} else {
				reporter.sign(&payload)
			});

			if let Some(signature) = maybe_signature {
				let signed = indices::address::Address::<app::Public, T::Index>::Id(reporter.clone());
				let extrinsic = UncheckedExtrinsic::new_signed(
					raw_payload.0,
					signed,
					signature,
					extra,
				).encode();

				return Some(extrinsic.encode())
			}
		}

		None
	}

	fn deposit_consensus<U: Encode>(new: U) {
		let log: DigestItem<T::Hash> = DigestItem::Consensus(BABE_ENGINE_ID, new.encode());
		<system::Module<T>>::deposit_log(log.into())
	}

	fn get_inherent_digests() -> system::DigestOf<T> {
		<system::Module<T>>::digest()
	}

	fn deposit_vrf_output(vrf_output: &[u8; VRF_OUTPUT_LENGTH]) {
		let segment_idx = <SegmentIndex>::get();
		let mut segment = <UnderConstruction>::get(&segment_idx);
		if segment.len() < UNDER_CONSTRUCTION_SEGMENT_LENGTH {
			// push onto current segment: not full.
			segment.push(*vrf_output);
			<UnderConstruction>::insert(&segment_idx, &segment);
		} else {
			// move onto the next segment and update the index.
			let segment_idx = segment_idx + 1;
			<UnderConstruction>::insert(&segment_idx, vec![*vrf_output].as_ref());
			<SegmentIndex>::put(&segment_idx);
		}
	}

	fn do_initialize() {
		// since do_initialize can be called twice (if session module is present)
		// => let's ensure that we only modify the storage once per block
		let initialized = Self::initialized().unwrap_or(false);
		if initialized {
			return;
		}

		Initialized::put(true);
		for digest in Self::get_inherent_digests()
			.logs
			.iter()
			.filter_map(|s| s.as_pre_runtime())
			.filter_map(|(id, mut data)| if id == BABE_ENGINE_ID {
				RawBabePreDigest::decode(&mut data).ok()
			} else {
				None
			})
		{
			if EpochStartSlot::get() == 0 {
				EpochStartSlot::put(digest.slot_number());
			}

			CurrentSlot::put(digest.slot_number());

			if let RawBabePreDigest::Primary { vrf_output, .. } = digest {
				Self::deposit_vrf_output(&vrf_output);
			}

			return;
		}
	}

	/// Call this function exactly once when an epoch changes, to update the
	/// randomness. Returns the new randomness.
	fn randomness_change_epoch(next_epoch_index: u64) -> [u8; RANDOMNESS_LENGTH] {
		let this_randomness = NextRandomness::get();
		let segment_idx: u32 = <SegmentIndex>::mutate(|s| rstd::mem::replace(s, 0));

		// overestimate to the segment being full.
		let rho_size = segment_idx.saturating_add(1) as usize * UNDER_CONSTRUCTION_SEGMENT_LENGTH;

		let next_randomness = compute_randomness(
			this_randomness,
			next_epoch_index,
			(0..segment_idx).flat_map(|i| <UnderConstruction>::take(&i)),
			Some(rho_size),
		);
		NextRandomness::put(&next_randomness);
		this_randomness
	}

	fn initialize_authorities(authorities: &[(AuthorityId, BabeAuthorityWeight)]) {
		if !authorities.is_empty() {
			assert!(Authorities::get().is_empty(), "Authorities are already initialized!");
			Authorities::put_ref(authorities);
		}
	}
}

impl<T: Trait> OnTimestampSet<T::Moment> for Module<T> {
	fn on_timestamp_set(_moment: T::Moment) { }
}

impl<T: Trait> session::OneSessionHandler<T::AccountId> for Module<T> {
	type Key = AuthorityId;

	fn on_genesis_session<'a, I: 'a>(validators: I)
		where I: Iterator<Item=(&'a T::AccountId, AuthorityId)>
	{
		let authorities = validators.map(|(_, k)| (k, 1)).collect::<Vec<_>>();
		Self::initialize_authorities(&authorities);
	}

	fn on_new_session<'a, I: 'a>(_changed: bool, validators: I, queued_validators: I)
		where I: Iterator<Item=(&'a T::AccountId, AuthorityId)>
	{
		// Update epoch index
		let epoch_index = EpochIndex::get()
			.checked_add(1)
			.expect("epoch indices will never reach 2^64 before the death of the universe; qed");

		EpochIndex::put(epoch_index);

		// Update authorities.
		let authorities = validators.map(|(_account, k)| {
			(k, 1)
		}).collect::<Vec<_>>();

		Authorities::put(authorities);

		// Update epoch start slot.
		let now = CurrentSlot::get();
		EpochStartSlot::mutate(|previous| {
			loop {
				// on the first epoch we must account for skipping at least one
				// whole epoch, in case the first block is authored with a slot
				// number far in the past.
				if now.saturating_sub(*previous) < T::EpochDuration::get() {
					break;
				}

				*previous = previous.saturating_add(T::EpochDuration::get());
			}
		});

		// Update epoch randomness.
		let next_epoch_index = epoch_index
			.checked_add(1)
			.expect("epoch indices will never reach 2^64 before the death of the universe; qed");

		// Returns randomness for the current epoch and computes the *next*
		// epoch randomness.
		let randomness = Self::randomness_change_epoch(next_epoch_index);
		Randomness::put(randomness);

		// After we update the current epoch, we signal the *next* epoch change
		// so that nodes can track changes.
		let next_authorities = queued_validators.map(|(_account, k)| {
			(k, 1)
		}).collect::<Vec<_>>();

		let next_epoch_start_slot = EpochStartSlot::get().saturating_add(T::EpochDuration::get());
		let next_randomness = NextRandomness::get();

		// Update any pending secondary slots change
		let mut secondary_slots = SecondarySlots::get();

		// change for E + 1 now becomes change at E
		secondary_slots.0 = secondary_slots.1;

		if let Some(change) = PendingSecondarySlotsChange::take() {
			// if there's a pending change schedule it for E + 1
			secondary_slots.1 = change;
		} else {
			// otherwise E + 1 will have the same value as E
			secondary_slots.1 = secondary_slots.0;
		}

		SecondarySlots::mutate(|secondary| {
			*secondary = secondary_slots;
		});

		let next = Epoch {
			epoch_index: next_epoch_index,
			start_slot: next_epoch_start_slot,
			duration: T::EpochDuration::get(),
			authorities: next_authorities,
			randomness: next_randomness,
			secondary_slots: secondary_slots.1,
		};

		Self::deposit_consensus(ConsensusLog::NextEpochData(next))
	}

	fn on_disabled(i: usize) {
		Self::deposit_consensus(ConsensusLog::OnDisabled(i as u32))
	}
}

// compute randomness for a new epoch. rho is the concatenation of all
// VRF outputs in the prior epoch.
//
// an optional size hint as to how many VRF outputs there were may be provided.
fn compute_randomness(
	last_epoch_randomness: [u8; RANDOMNESS_LENGTH],
	epoch_index: u64,
	rho: impl Iterator<Item=[u8; VRF_OUTPUT_LENGTH]>,
	rho_size_hint: Option<usize>,
) -> [u8; RANDOMNESS_LENGTH] {
	let mut s = Vec::with_capacity(40 + rho_size_hint.unwrap_or(0) * VRF_OUTPUT_LENGTH);
	s.extend_from_slice(&last_epoch_randomness);
	s.extend_from_slice(&epoch_index.to_le_bytes());

	for vrf_output in rho {
		s.extend_from_slice(&vrf_output[..]);
	}

	sr_io::blake2_256(&s)
}

impl<T: Trait> ProvideInherent for Module<T> {
	type Call = timestamp::Call<T>;
	type Error = MakeFatalError<RuntimeString>;
	const INHERENT_IDENTIFIER: InherentIdentifier = INHERENT_IDENTIFIER;

	fn create_inherent(_: &InherentData) -> Option<Self::Call> {
		None
	}

	fn check_inherent(call: &Self::Call, data: &InherentData) -> result::Result<(), Self::Error> {
		let timestamp = match call {
			timestamp::Call::set(ref timestamp) => timestamp.clone(),
			_ => return Ok(()),
		};

		let timestamp_based_slot = (timestamp / Self::slot_duration()).saturated_into::<u64>();
		let seal_slot = data.babe_inherent_data()?;

		if timestamp_based_slot == seal_slot {
			Ok(())
		} else {
			Err(RuntimeString::from("timestamp set in block doesn't match slot in seal").into())
		}
	}
}
