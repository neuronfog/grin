// Copyright 2018 The Grin Developers
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

extern crate blake2_rfc as blake2;
extern crate grin_chain as chain;
extern crate grin_core as core;
extern crate grin_keychain as keychain;
extern crate grin_pool as pool;
extern crate grin_util as util;
extern crate grin_wallet as wallet;

extern crate rand;
extern crate chrono;

pub mod common;

use std::sync::{Arc, RwLock};

use chain::types::Tip;
use chain::{txhashset, ChainStore};
use common::{clean_output_dir, test_setup, test_source, test_transaction,
             test_transaction_spending_coinbase, ChainAdapter};
use core::core::target::Difficulty;
use core::core::{transaction, Block, BlockHeader};
use keychain::{ExtKeychain, Keychain};
use wallet::libtx;

/// Test we can add some txs to the pool (both stempool and txpool).
#[test]
fn test_the_transaction_pool() {
	let keychain: ExtKeychain = Keychain::from_random_seed().unwrap();

	let db_root = ".grin_transaction_pool".to_string();
	clean_output_dir(db_root.clone());
	let chain = ChainAdapter::init(db_root.clone()).unwrap();

	// Initialize the chain/txhashset with a few blocks,
	// so we have a non-empty UTXO set.
	let header = {
		let height = 1;
		let key_id = keychain.derive_key_id(height as u32).unwrap();
		let reward = libtx::reward::output(&keychain, &key_id, 0, height).unwrap();
		let block = Block::new(&BlockHeader::default(), vec![], Difficulty::one(), reward).unwrap();

		let mut txhashset = chain.txhashset.write().unwrap();
		let mut batch = chain.store.batch().unwrap();
		txhashset::extending(&mut txhashset, &mut batch, |extension| {
			extension.apply_block(&block)
		}).unwrap();

		let tip = Tip::from_block(&block.header);
		batch.save_block_header(&block.header).unwrap();
		batch.save_head(&tip).unwrap();
		batch.commit().unwrap();

		block.header
	};

	// Initialize a new pool with our chain adapter.
	let pool = RwLock::new(test_setup(&Arc::new(chain.clone())));

	// Now create tx to spend a coinbase, giving us some useful outputs for testing
	// with.
	let initial_tx = {
		test_transaction_spending_coinbase(
			&keychain,
			&header,
			vec![500, 600, 700, 800, 900, 1000, 1100, 1200, 1300, 1400],
		)
	};

	// Add this tx to the pool (stem=false, direct to txpool).
	{
		let mut write_pool = pool.write().unwrap();
		write_pool
			.add_to_pool(test_source(), initial_tx, false)
			.unwrap();
		assert_eq!(write_pool.total_size(), 1);
	}

	// tx1 spends some outputs from the initial test tx.
	let tx1 = test_transaction(&keychain, vec![500, 600], vec![499, 599]);
	// tx2 spends some outputs from both tx1 and the initial test tx.
	let tx2 = test_transaction(&keychain, vec![499, 700], vec![498]);

	// Take a write lock and add a couple of tx entries to the pool.
	{
		let mut write_pool = pool.write().unwrap();

		// Check we have a single initial tx in the pool.
		assert_eq!(write_pool.total_size(), 1);

		// First, add a simple tx to the pool in "stem" mode.
		write_pool
			.add_to_pool(test_source(), tx1.clone(), true)
			.unwrap();
		assert_eq!(write_pool.total_size(), 1);
		assert_eq!(write_pool.stempool.size(), 1);

		// Add another tx spending outputs from the previous tx.
		write_pool
			.add_to_pool(test_source(), tx2.clone(), true)
			.unwrap();
		assert_eq!(write_pool.total_size(), 1);
		assert_eq!(write_pool.stempool.size(), 2);
	}

	// Test adding the exact same tx multiple times (same kernel signature).
	// This will fail during tx aggregation due to duplicate outputs and duplicate
	// kernels.
	{
		let mut write_pool = pool.write().unwrap();
		assert!(
			write_pool
				.add_to_pool(test_source(), tx1.clone(), true)
				.is_err()
		);
	}

	// Test adding a duplicate tx with the same input and outputs (not the *same*
	// tx).
	{
		let tx1a = test_transaction(&keychain, vec![500, 600], vec![499, 599]);
		let mut write_pool = pool.write().unwrap();
		assert!(write_pool.add_to_pool(test_source(), tx1a, true).is_err());
	}

	// Test adding a tx attempting to spend a non-existent output.
	{
		let bad_tx = test_transaction(&keychain, vec![10_001], vec![10_000]);
		let mut write_pool = pool.write().unwrap();
		assert!(write_pool.add_to_pool(test_source(), bad_tx, true).is_err());
	}

	// Test adding a tx that would result in a duplicate output (conflicts with
	// output from tx2). For reasons of security all outputs in the UTXO set must
	// be unique. Otherwise spending one will almost certainly cause the other
	// to be immediately stolen via a "replay" tx.
	{
		let tx = test_transaction(&keychain, vec![900], vec![498]);
		let mut write_pool = pool.write().unwrap();
		assert!(write_pool.add_to_pool(test_source(), tx, true).is_err());
	}

	// Confirm the tx pool correctly identifies an invalid tx (already spent).
	{
		let mut write_pool = pool.write().unwrap();
		let tx3 = test_transaction(&keychain, vec![500], vec![497]);
		assert!(write_pool.add_to_pool(test_source(), tx3, true).is_err());
		assert_eq!(write_pool.total_size(), 1);
		assert_eq!(write_pool.stempool.size(), 2);
	}

	// Check we can take some entries from the stempool and "fluff" them into the
	// txpool. This also exercises multi-kernel txs.
	{
		let mut write_pool = pool.write().unwrap();
		let agg_tx = write_pool
			.stempool
			.aggregate_transaction()
			.unwrap()
			.unwrap();
		assert_eq!(agg_tx.kernels.len(), 2);
		write_pool
			.add_to_pool(test_source(), agg_tx, false)
			.unwrap();
		assert_eq!(write_pool.total_size(), 2);
	}

	// Now check we can correctly deaggregate a multi-kernel tx based on current
	// contents of the txpool.
	// We will do this be adding a new tx to the pool
	// that is a superset of a tx already in the pool.
	{
		let mut write_pool = pool.write().unwrap();

		let tx4 = test_transaction(&keychain, vec![800], vec![799]);
		// tx1 and tx2 are already in the txpool (in aggregated form)
		// tx4 is the "new" part of this aggregated tx that we care about
		let agg_tx = transaction::aggregate(vec![tx1.clone(), tx2.clone(), tx4]).unwrap();
		write_pool
			.add_to_pool(test_source(), agg_tx, false)
			.unwrap();
		assert_eq!(write_pool.total_size(), 3);
		let entry = write_pool.txpool.entries.last().unwrap();
		assert_eq!(entry.tx.kernels.len(), 1);
		assert_eq!(entry.src.debug_name, "deagg");
	}

	// Check we cannot "double spend" an output spent in a previous block.
	// We use the initial coinbase output here for convenience.
	{
		let mut write_pool = pool.write().unwrap();

		let double_spend_tx =
			{ test_transaction_spending_coinbase(&keychain, &header, vec![1000]) };

		// check we cannot add a double spend to the stempool
		assert!(
			write_pool
				.add_to_pool(test_source(), double_spend_tx.clone(), true)
				.is_err()
		);

		// check we cannot add a double spend to the txpool
		assert!(
			write_pool
				.add_to_pool(test_source(), double_spend_tx.clone(), false)
				.is_err()
		);
	}
}
