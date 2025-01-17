// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Cumulus.

// Cumulus is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Cumulus is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Cumulus.  If not, see <http://www.gnu.org/licenses/>.

#![cfg(feature = "runtime-benchmarks")]

use super::*;
use frame_support::pallet_prelude::{DispatchClass, Pays};
use frame_system::RawOrigin;
use sp_runtime::traits::{AsTransactionAuthorizedOrigin, DispatchTransaction};

#[frame_benchmarking::v2::benchmarks(
	where T: Send + Sync,
		<T as frame_system::Config>::RuntimeCall:
			Dispatchable<Info = DispatchInfo, PostInfo = PostDispatchInfo>,
		<T as frame_system::Config>::RuntimeOrigin: AsTransactionAuthorizedOrigin,
)]
mod bench {
	use super::*;
	use frame_benchmarking::impl_test_function;

	#[benchmark]
	fn storage_weight_reclaim() {
		let ext = StorageWeightReclaim::<T, ()>::new(());

		let origin = RawOrigin::Root.into();
		let call = T::RuntimeCall::from(frame_system::Call::remark { remark: alloc::vec![] });

		let overestimate = 10_000;
		let info = DispatchInfo {
			call_weight: Weight::zero().add_proof_size(overestimate),
			extension_weight: Weight::zero(),
			class: DispatchClass::Normal,
			pays_fee: Pays::No,
		};

		let post_info = PostDispatchInfo { actual_weight: None, pays_fee: Pays::No };

		let mut block_weight = frame_system::ConsumedWeight::default();
		block_weight.accrue(Weight::from_parts(0, overestimate), info.class);

		frame_system::BlockWeight::<T>::put(block_weight);

		#[block]
		{
			assert!(ext.test_run(origin, &call, &info, 0, 0, |_| Ok(post_info)).unwrap().is_ok());
		}

		let final_block_proof_size =
			frame_system::BlockWeight::<T>::get().get(info.class).proof_size();

		assert!(
			final_block_proof_size < overestimate,
			"The proof size measured should be less than {overestimate}"
		);
	}

	impl_benchmark_test_suite!(Pallet, crate::tests::setup_test_ext_default(), crate::tests::Test);
}
