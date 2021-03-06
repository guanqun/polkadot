// Copyright 2017 Parity Technologies (UK) Ltd.
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

//! Executive: Handles all of the top-level stuff; essentially just executing blocks/extrinsics.

#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(feature = "std")]
extern crate serde;
#[cfg(test)]
#[macro_use]
extern crate serde_derive;

#[cfg(test)]
#[macro_use]
extern crate parity_codec_derive;

#[cfg_attr(test, macro_use)]
extern crate srml_support as runtime_support;

extern crate sr_std as rstd;
extern crate sr_io as runtime_io;
extern crate parity_codec as codec;
extern crate sr_primitives as primitives;
extern crate srml_system as system;

#[cfg(test)]
#[macro_use]
extern crate hex_literal;

#[cfg(test)]
extern crate substrate_primitives;

#[cfg(test)]
extern crate srml_balances as balances;

use rstd::prelude::*;
use rstd::marker::PhantomData;
use rstd::result;
use primitives::traits::{self, Header, Zero, One, Checkable, Applyable, CheckEqual, OnFinalise,
	MakePayment, Hash};
use runtime_support::Dispatchable;
use codec::{Codec, Encode};
use system::extrinsics_root;
use primitives::{ApplyOutcome, ApplyError};

mod internal {
	pub enum ApplyError {
		BadSignature(&'static str),
		Stale,
		Future,
		CantPay,
	}

	pub enum ApplyOutcome {
		Success,
		Fail(&'static str),
	}
}

pub struct Executive<
	System,
	Block,
	Lookup,
	Payment,
	Finalisation,
>(PhantomData<(System, Block, Lookup, Payment, Finalisation)>);

impl<
	Address,
	System: system::Trait,
	Block: traits::Block<Header=System::Header, Hash=System::Hash>,
	Lookup: traits::Lookup<Source=Address, Target=System::AccountId>,
	Payment: MakePayment<System::AccountId>,
	Finalisation: OnFinalise<System::BlockNumber>,
> Executive<System, Block, Lookup, Payment, Finalisation> where
	Block::Extrinsic: Checkable<fn(Address) -> Result<System::AccountId, &'static str>> + Codec,
	<Block::Extrinsic as Checkable<fn(Address) -> Result<System::AccountId, &'static str>>>::Checked: Applyable<Index=System::Index, AccountId=System::AccountId>,
	<<Block::Extrinsic as Checkable<fn(Address) -> Result<System::AccountId, &'static str>>>::Checked as Applyable>::Call: Dispatchable,
	<<<Block::Extrinsic as Checkable<fn(Address) -> Result<System::AccountId, &'static str>>>::Checked as Applyable>::Call as Dispatchable>::Origin: From<Option<System::AccountId>>
{
	/// Start the execution of a particular block.
	pub fn initialise_block(header: &System::Header) {
		<system::Module<System>>::initialise(header.number(), header.parent_hash(), header.extrinsics_root());
	}

	fn initial_checks(block: &Block) {
		let header = block.header();

		// check parent_hash is correct.
		let n = header.number().clone();
		assert!(
			n > System::BlockNumber::zero() && <system::Module<System>>::block_hash(n - System::BlockNumber::one()) == *header.parent_hash(),
			"Parent hash should be valid."
		);

		// check transaction trie root represents the transactions.
		let xts_root = extrinsics_root::<System::Hashing, _>(&block.extrinsics());
		header.extrinsics_root().check_equal(&xts_root);
		assert!(header.extrinsics_root() == &xts_root, "Transaction trie root must be valid.");
	}

	/// Actually execute all transitioning for `block`.
	pub fn execute_block(block: Block) {
		Self::initialise_block(block.header());

		// any initial checks
		Self::initial_checks(&block);

		// execute transactions
		let (header, extrinsics) = block.deconstruct();
		extrinsics.into_iter().for_each(Self::apply_extrinsic_no_note);

		// post-transactional book-keeping.
		<system::Module<System>>::note_finished_extrinsics();
		Finalisation::on_finalise(*header.number());

		// any final checks
		Self::final_checks(&header);
	}

	/// Finalise the block - it is up the caller to ensure that all header fields are valid
	/// except state-root.
	pub fn finalise_block() -> System::Header {
		<system::Module<System>>::note_finished_extrinsics();
		Finalisation::on_finalise(<system::Module<System>>::block_number());

		// setup extrinsics
		<system::Module<System>>::derive_extrinsics();
		<system::Module<System>>::finalise()
	}

	/// Apply extrinsic outside of the block execution function.
	/// This doesn't attempt to validate anything regarding the block, but it builds a list of uxt
	/// hashes.
	pub fn apply_extrinsic(uxt: Block::Extrinsic) -> result::Result<ApplyOutcome, ApplyError> {
		let encoded = uxt.encode();
		let encoded_len = encoded.len();
		<system::Module<System>>::note_extrinsic(encoded);
		match Self::apply_extrinsic_no_note_with_len(uxt, encoded_len) {
			Ok(internal::ApplyOutcome::Success) => Ok(ApplyOutcome::Success),
			Ok(internal::ApplyOutcome::Fail(_)) => Ok(ApplyOutcome::Fail),
			Err(internal::ApplyError::CantPay) => Err(ApplyError::CantPay),
			Err(internal::ApplyError::BadSignature(_)) => Err(ApplyError::BadSignature),
			Err(internal::ApplyError::Stale) => Err(ApplyError::Stale),
			Err(internal::ApplyError::Future) => Err(ApplyError::Future),
		}
	}

	/// Apply an extrinsic inside the block execution function.
	fn apply_extrinsic_no_note(uxt: Block::Extrinsic) {
		let l = uxt.encode().len();
		match Self::apply_extrinsic_no_note_with_len(uxt, l) {
			Ok(internal::ApplyOutcome::Success) => (),
			Ok(internal::ApplyOutcome::Fail(e)) => runtime_io::print(e),
			Err(internal::ApplyError::CantPay) => panic!("All extrinsics should have sender able to pay their fees"),
			Err(internal::ApplyError::BadSignature(_)) => panic!("All extrinsics should be properly signed"),
			Err(internal::ApplyError::Stale) | Err(internal::ApplyError::Future) => panic!("All extrinsics should have the correct nonce"),
		}
	}

	/// Actually apply an extrinsic given its `encoded_len`; this doesn't note its hash.
	fn apply_extrinsic_no_note_with_len(uxt: Block::Extrinsic, encoded_len: usize) -> result::Result<internal::ApplyOutcome, internal::ApplyError> {
		// Verify the signature is good.
		let xt = uxt.check_with(Lookup::lookup).map_err(internal::ApplyError::BadSignature)?;

		if let Some(sender) = xt.sender() {
			// check index
			let expected_index = <system::Module<System>>::account_nonce(sender);
			if xt.index() != &expected_index { return Err(
				if xt.index() < &expected_index { internal::ApplyError::Stale } else { internal::ApplyError::Future }
			) }

			// pay any fees.
			Payment::make_payment(sender, encoded_len).map_err(|_| internal::ApplyError::CantPay)?;

			// AUDIT: Under no circumstances may this function panic from here onwards.

			// increment nonce in storage
			<system::Module<System>>::inc_account_nonce(sender);
		}

		// decode parameters and dispatch
		let (f, s) = xt.deconstruct();
		let r = f.dispatch(s.into());
		<system::Module<System>>::note_applied_extrinsic(&r);

		r.map(|_| internal::ApplyOutcome::Success).or_else(|e| Ok(internal::ApplyOutcome::Fail(e)))
	}

	fn final_checks(header: &System::Header) {
		// check digest
		assert!(header.digest() == &<system::Module<System>>::digest());

		// remove temporaries.
		<system::Module<System>>::finalise();

		// check storage root.
		let storage_root = System::Hashing::storage_root();
		header.state_root().check_equal(&storage_root);
		assert!(header.state_root() == &storage_root, "Storage root must match that calculated.");
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use balances::Call;
	use runtime_io::with_externalities;
	use substrate_primitives::{H256, Blake2Hasher};
	use primitives::BuildStorage;
	use primitives::traits::{Header as HeaderT, BlakeTwo256, Lookup};
	use primitives::testing::{Digest, Header, Block};
	use system;

	struct NullLookup;
	impl Lookup for NullLookup {
		type Source = u64;
		type Target = u64;
		fn lookup(s: Self::Source) -> Result<Self::Target, &'static str> {
			Ok(s)
		}
	}

	impl_outer_origin! {
		pub enum Origin for Runtime {
		}
	}

	impl_outer_event!{
		pub enum MetaEvent for Runtime {
			balances<T>,
		}
	}

	// Workaround for https://github.com/rust-lang/rust/issues/26925 . Remove when sorted.
	#[derive(Clone, Eq, PartialEq, Debug, Serialize, Deserialize)]
	pub struct Runtime;
	impl system::Trait for Runtime {
		type Origin = Origin;
		type Index = u64;
		type BlockNumber = u64;
		type Hash = substrate_primitives::H256;
		type Hashing = BlakeTwo256;
		type Digest = Digest;
		type AccountId = u64;
		type Header = Header;
		type Event = MetaEvent;
	}
	impl balances::Trait for Runtime {
		type Balance = u64;
		type AccountIndex = u64;
		type OnFreeBalanceZero = ();
		type EnsureAccountLiquid = ();
		type Event = MetaEvent;
	}

	type TestXt = primitives::testing::TestXt<Call<Runtime>>;
	type Executive = super::Executive<Runtime, Block<TestXt>, NullLookup, balances::Module<Runtime>, ()>;

	#[test]
	fn balance_transfer_dispatch_works() {
		let mut t = system::GenesisConfig::<Runtime>::default().build_storage().unwrap();
		t.extend(balances::GenesisConfig::<Runtime> {
			balances: vec![(1, 111)],
			transaction_base_fee: 10,
			transaction_byte_fee: 0,
			existential_deposit: 0,
			transfer_fee: 0,
			creation_fee: 0,
			reclaim_rebate: 0,
		}.build_storage().unwrap());
		let xt = primitives::testing::TestXt(Some(1), 0, Call::transfer(2.into(), 69));
		let mut t = runtime_io::TestExternalities::from(t);
		with_externalities(&mut t, || {
			Executive::initialise_block(&Header::new(1, H256::default(), H256::default(), [69u8; 32].into(), Digest::default()));
			Executive::apply_extrinsic(xt).unwrap();
			assert_eq!(<balances::Module<Runtime>>::total_balance(&1), 32);
			assert_eq!(<balances::Module<Runtime>>::total_balance(&2), 69);
		});
	}

	fn new_test_ext() -> runtime_io::TestExternalities<Blake2Hasher> {
		let mut t = system::GenesisConfig::<Runtime>::default().build_storage().unwrap();
		t.extend(balances::GenesisConfig::<Runtime>::default().build_storage().unwrap());
		t.into()
	}

	#[test]
	fn block_import_works() {
		with_externalities(&mut new_test_ext(), || {
			Executive::execute_block(Block {
				header: Header {
					parent_hash: [69u8; 32].into(),
					number: 1,
					state_root: hex!("d1d3da2b1efb1a6ef740b8cdef52e4cf3c6dade6f8a360969fd7ef0034c53b54").into(),
					extrinsics_root: hex!("45b0cfc220ceec5b7c1c62c4d4193d38e4eba48e8815729ce75f9c0ab0e4c1c0").into(),
					digest: Digest { logs: vec![], },
				},
				extrinsics: vec![],
			});
		});
	}

	#[test]
	#[should_panic]
	fn block_import_of_bad_state_root_fails() {
		with_externalities(&mut new_test_ext(), || {
			Executive::execute_block(Block {
				header: Header {
					parent_hash: [69u8; 32].into(),
					number: 1,
					state_root: [0u8; 32].into(),
					extrinsics_root: hex!("45b0cfc220ceec5b7c1c62c4d4193d38e4eba48e8815729ce75f9c0ab0e4c1c0").into(),
					digest: Digest { logs: vec![], },
				},
				extrinsics: vec![],
			});
		});
	}

	#[test]
	#[should_panic]
	fn block_import_of_bad_extrinsic_root_fails() {
		with_externalities(&mut new_test_ext(), || {
			Executive::execute_block(Block {
				header: Header {
					parent_hash: [69u8; 32].into(),
					number: 1,
					state_root: hex!("d1d3da2b1efb1a6ef740b8cdef52e4cf3c6dade6f8a360969fd7ef0034c53b54").into(),
					extrinsics_root: [0u8; 32].into(),
					digest: Digest { logs: vec![], },
				},
				extrinsics: vec![],
			});
		});
	}

	#[test]
	fn bad_extrinsic_not_inserted() {
		let mut t = new_test_ext();
		let xt = primitives::testing::TestXt(Some(1), 42, Call::transfer(33.into(), 69));
		with_externalities(&mut t, || {
			Executive::initialise_block(&Header::new(1, H256::default(), H256::default(), [69u8; 32].into(), Digest::default()));
			assert!(Executive::apply_extrinsic(xt).is_err());
			assert_eq!(<system::Module<Runtime>>::extrinsic_index(), Some(0));
		});
	}
}
