// This file is part of Substrate.

// Copyright (C) 2018-2021 Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! A module that implements instrumented code cache.
//!
//! - In order to run contract code we need to instrument it with gas metering.
//! To do that we need to provide the schedule which will supply exact gas costs values.
//! We cache this code in the storage saving the schedule version.
//! - Before running contract code we check if the cached code has the schedule version that
//! is equal to the current saved schedule.
//! If it is equal then run the code, if it isn't reinstrument with the current schedule.
//! - When we update the schedule we want it to have strictly greater version than the current saved one:
//! this guarantees that every instrumented contract code in cache cannot have the version equal to the current one.
//! Thus, before executing a contract it should be reinstrument with new schedule.

use crate::{
	CodeHash, CodeStorage, PristineCode, Schedule, Config, Error,
	wasm::{prepare, PrefabWasmModule}, Module as Contracts, RawEvent,
};
use sp_core::crypto::UncheckedFrom;
use frame_support::{StorageMap, dispatch::{DispatchError, DispatchResult}};

/// Put the instrumented module in storage.
///
/// Increments the refcount of the in-storage `prefab_module` if it already exists in storage
/// under the specified `code_hash`.
pub fn store<T: Config>(mut prefab_module: PrefabWasmModule<T>)
where
	T::AccountId: UncheckedFrom<T::Hash> + AsRef<[u8]>
{
	let code_hash = sp_std::mem::take(&mut prefab_module.code_hash);
	// code_hash is only `Some` if the contract was instantiated from a new code
	// but `None` if it was loaded from storage.
	if let Some(code) = prefab_module.original_code.take() {
		<PristineCode<T>>::insert(&code_hash, code);
	}
	<CodeStorage<T>>::mutate(&code_hash, |existing| {
		match existing {
			// We try hard to be infallible here because otherwise more storage
			// transactions would be necessary to account for failures in storing
			// code for an already instantiated contract.
			Some(module) => safe_increment(&mut module.refcount),
			None => {
				*existing = Some(prefab_module);
				Contracts::<T>::deposit_event(RawEvent::CodeStored(code_hash))
			}
		}
	});
}

pub fn increment_refcount<T: Config>(code_hash: CodeHash<T>) -> DispatchResult
where
	T::AccountId: UncheckedFrom<T::Hash> + AsRef<[u8]>
{
	<CodeStorage<T>>::mutate(code_hash, |existing| {
		if let Some(module) = existing {
			safe_increment(&mut module.refcount);
			Ok(())
		} else {
			Err(Error::<T>::CodeNotFound.into())
		}
	})
}

pub fn decrement_refcount<T: Config>(code_hash: CodeHash<T>)
where
	T::AccountId: UncheckedFrom<T::Hash> + AsRef<[u8]>
{
	<CodeStorage<T>>::mutate_exists(code_hash, |existing| {
		if let Some(module) = existing {
			module.refcount = module.refcount.saturating_sub(1);
			if module.refcount == 0 {
				*existing = None;
				<PristineCode<T>>::remove(code_hash);
				Contracts::<T>::deposit_event(RawEvent::CodeRemoved(code_hash));
			}
		}
	});
}

/// Prepare and save the code to storage in one go.
//
/// This version neither checks nor instruments the passed in code. This is useful
/// when code needs to be benchmarked without the injected instrumentation.
///
/// # Note
///
/// In production code the preparation and storage are seperate steps because
/// we only want to store in case of a successful instantiation. For benchmarks we can
/// disregard this error case.
#[cfg(feature = "runtime-benchmarks")]
pub fn prepare_and_store_unchecked<T: Config>(
	original_code: Vec<u8>,
	schedule: &Schedule<T>,
) -> Result<CodeHash<T>, &'static str> where T::AccountId: UncheckedFrom<T::Hash> + AsRef<[u8]> {
	let prefab_module = prepare::benchmarking::prepare_contract::<T>(&original_code, schedule)?;
	let code_hash = T::Hashing::hash(&original_code);
	store(prefab_module, &code_hash);
	Ok(code_hash)
}

/// Load code with the given code hash.
///
/// If the module was instrumented with a lower version of schedule than
/// the current one given as an argument, then this function will perform
/// re-instrumentation and update the cache in the storage.
pub fn load<T: Config>(
	code_hash: CodeHash<T>,
	schedule: &Schedule<T>,
) -> Result<PrefabWasmModule<T>, DispatchError>
where
	T::AccountId: UncheckedFrom<T::Hash> + AsRef<[u8]>
{
	let mut prefab_module = <CodeStorage<T>>::get(code_hash)
		.ok_or_else(|| Error::<T>::CodeNotFound)?;

	if prefab_module.schedule_version < schedule.version {
		// The current schedule version is greater than the version of the one cached
		// in the storage.
		//
		// We need to re-instrument the code with the latest schedule here.
		let original_code = <PristineCode<T>>::get(code_hash)
			.ok_or_else(|| Error::<T>::CodeNotFound)?;
		prefab_module = prepare::prepare_contract::<T>(original_code, schedule)?;
		<CodeStorage<T>>::insert(&code_hash, &prefab_module);
	}
	prefab_module.code_hash = code_hash;
	Ok(prefab_module)
}

fn safe_increment(refcount: &mut u64) {
	*refcount = refcount.checked_add(1).expect("
		refcount is 64bit. Generating this overflow would require to store
		_at least_ 18 exabyte of data assuming that a contract consumes only
		one byte of data. Any node would run out of storage space before hitting
		this overflow.
		qed
	");
}
