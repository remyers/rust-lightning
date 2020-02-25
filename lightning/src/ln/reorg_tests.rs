//! Further functional tests which test blockchain reorganizations.

use ln::channelmonitor::ANTI_REORG_DELAY;
use ln::features::InitFeatures;
use ln::msgs::{ChannelMessageHandler, ErrorAction, HTLCFailChannelUpdate};
use util::events::{Event, EventsProvider, MessageSendEvent, MessageSendEventsProvider};

use bitcoin::util::hash::BitcoinHash;
use bitcoin::blockdata::block::{Block, BlockHeader};

use std::default::Default;

use ln::functional_test_utils::*;

fn do_test_onchain_htlc_reorg(local_commitment: bool, claim: bool) {
	// Our on-chain HTLC-claim learning has a few properties worth testing:
	//  * If an upstream HTLC is claimed with a preimage (both against our own commitment
	//    transaction our counterparty's), we claim it backwards immediately.
	//  * If an upstream HTLC is claimed with a timeout, we delay ANTI_REORG_DELAY before failing
	//    it backwards to ensure our counterparty can't claim with a preimage in a reorg.
	//
	// Here we test both properties in any combination based on the two bools passed in as
	// arguments.
	//
	// If local_commitment is set, we first broadcast a local commitment containing an offered HTLC
	// and an HTLC-Timeout tx, otherwise we broadcast a remote commitment containing a received
	// HTLC and a local HTLC-Timeout tx spending it.
	//
	// We then either allow these transactions to confirm (if !claim) or we wait until one block
	// before they otherwise would and reorg them out, confirming an HTLC-Success tx instead.
	let node_cfgs = create_node_cfgs(3);
	let node_chanmgrs = create_node_chanmgrs(3, &node_cfgs, &[None, None, None]);
	let nodes = create_network(3, &node_cfgs, &node_chanmgrs);

	create_announced_chan_between_nodes(&nodes, 0, 1, InitFeatures::supported(), InitFeatures::supported());
	let chan_2 = create_announced_chan_between_nodes(&nodes, 1, 2, InitFeatures::supported(), InitFeatures::supported());

	let (our_payment_preimage, our_payment_hash) = route_payment(&nodes[0], &[&nodes[1], &nodes[2]], 1000000);

	// Provide preimage to node 2 by claiming payment
	nodes[2].node.claim_funds(our_payment_preimage, &None, 1000000);
	check_added_monitors!(nodes[2], 1);
	get_htlc_update_msgs!(nodes[2], nodes[1].node.get_our_node_id());

	let mut headers = Vec::new();
	let mut header = BlockHeader { version: 0x2000_0000, prev_blockhash: Default::default(), merkle_root: Default::default(), time: 42, bits: 42, nonce: 42 };
	let claim_txn = if local_commitment {
		// Broadcast node 1 commitment txn to broadcast the HTLC-Timeout
		let node_1_commitment_txn = nodes[1].node.channel_state.lock().unwrap().by_id.get_mut(&chan_2.2).unwrap().channel_monitor().get_latest_local_commitment_txn();
		assert_eq!(node_1_commitment_txn.len(), 2); // 1 local commitment tx, 1 Outbound HTLC-Timeout
		assert_eq!(node_1_commitment_txn[0].output.len(), 2); // to-self and Offered HTLC (to-remote/to-node-3 is dust)
		check_spends!(node_1_commitment_txn[0], chan_2.3);
		check_spends!(node_1_commitment_txn[1], node_1_commitment_txn[0].clone());

		// Give node 2 node 1's transactions and get its response (claiming the HTLC instead).
		nodes[2].block_notifier.block_connected(&Block { header, txdata: node_1_commitment_txn.clone() }, CHAN_CONFIRM_DEPTH + 1);
		check_closed_broadcast!(nodes[2], false); // We should get a BroadcastChannelUpdate (and *only* a BroadcstChannelUpdate)
		let node_2_commitment_txn = nodes[2].tx_broadcaster.txn_broadcasted.lock().unwrap();
		assert_eq!(node_2_commitment_txn.len(), 3); // ChannelMonitor: 1 offered HTLC-Claim, ChannelManger: 1 local commitment tx, 1 Received HTLC-Claim
		assert_eq!(node_2_commitment_txn[1].output.len(), 2); // to-remote and Received HTLC (to-self is dust)
		check_spends!(node_2_commitment_txn[1], chan_2.3);
		check_spends!(node_2_commitment_txn[2], node_2_commitment_txn[1].clone());
		check_spends!(node_2_commitment_txn[0], node_1_commitment_txn[0]);

		// Confirm node 1's commitment txn (and HTLC-Timeout) on node 1
		nodes[1].block_notifier.block_connected(&Block { header, txdata: node_1_commitment_txn.clone() }, CHAN_CONFIRM_DEPTH + 1);

		// ...but return node 1's commitment tx in case claim is set and we're preparing to reorg
		vec![node_1_commitment_txn[0].clone(), node_2_commitment_txn[0].clone()]
	} else {
		// Broadcast node 2 commitment txn
		let node_2_commitment_txn = nodes[2].node.channel_state.lock().unwrap().by_id.get_mut(&chan_2.2).unwrap().channel_monitor().get_latest_local_commitment_txn();
		assert_eq!(node_2_commitment_txn.len(), 2); // 1 local commitment tx, 1 Received HTLC-Claim
		assert_eq!(node_2_commitment_txn[0].output.len(), 2); // to-remote and Received HTLC (to-self is dust)
		check_spends!(node_2_commitment_txn[0], chan_2.3);
		check_spends!(node_2_commitment_txn[1], node_2_commitment_txn[0].clone());

		// Give node 1 node 2's commitment transaction and get its response (timing the HTLC out)
		nodes[1].block_notifier.block_connected(&Block { header, txdata: vec![node_2_commitment_txn[0].clone()] }, CHAN_CONFIRM_DEPTH + 1);
		let node_1_commitment_txn = nodes[1].tx_broadcaster.txn_broadcasted.lock().unwrap();
		assert_eq!(node_1_commitment_txn.len(), 3); // ChannelMonitor: 1 offered HTLC-Timeout, ChannelManger: 1 local commitment tx, 1 Offered HTLC-Timeout
		assert_eq!(node_1_commitment_txn[1].output.len(), 2); // to-local and Offered HTLC (to-remote is dust)
		check_spends!(node_1_commitment_txn[1], chan_2.3);
		check_spends!(node_1_commitment_txn[2], node_1_commitment_txn[1].clone());
		check_spends!(node_1_commitment_txn[0], node_2_commitment_txn[0]);

		// Confirm node 2's commitment txn (and node 1's HTLC-Timeout) on node 1
		nodes[1].block_notifier.block_connected(&Block { header, txdata: vec![node_2_commitment_txn[0].clone(), node_1_commitment_txn[0].clone()] }, CHAN_CONFIRM_DEPTH + 1);
		// ...but return node 2's commitment tx (and claim) in case claim is set and we're preparing to reorg
		node_2_commitment_txn
	};
	check_closed_broadcast!(nodes[1], false); // We should get a BroadcastChannelUpdate (and *only* a BroadcstChannelUpdate)
	headers.push(header.clone());
	// At CHAN_CONFIRM_DEPTH + 1 we have a confirmation count of 1, so CHAN_CONFIRM_DEPTH +
	// ANTI_REORG_DELAY - 1 will give us a confirmation count of ANTI_REORG_DELAY - 1.
	for i in CHAN_CONFIRM_DEPTH + 2..CHAN_CONFIRM_DEPTH + ANTI_REORG_DELAY - 1 {
		header = BlockHeader { version: 0x20000000, prev_blockhash: header.bitcoin_hash(), merkle_root: Default::default(), time: 42, bits: 42, nonce: 42 };
		nodes[1].block_notifier.block_connected_checked(&header, i, &vec![], &[0; 0]);
		headers.push(header.clone());
	}
	check_added_monitors!(nodes[1], 0);
	assert_eq!(nodes[1].node.get_and_clear_pending_events().len(), 0);

	if claim {
		// Now reorg back to CHAN_CONFIRM_DEPTH and confirm node 2's broadcasted transactions:
		for (height, header) in (CHAN_CONFIRM_DEPTH + 1..CHAN_CONFIRM_DEPTH + ANTI_REORG_DELAY - 1).zip(headers.iter()).rev() {
			nodes[1].block_notifier.block_disconnected(&header, height);
		}

		header = BlockHeader { version: 0x20000000, prev_blockhash: Default::default(), merkle_root: Default::default(), time: 42, bits: 42, nonce: 42 };
		nodes[1].block_notifier.block_connected(&Block { header, txdata: claim_txn }, CHAN_CONFIRM_DEPTH + 1);

		// ChannelManager only polls ManyChannelMonitor::get_and_clear_pending_htlcs_updated when we
		// probe it for events, so we probe non-message events here (which should still end up empty):
		assert_eq!(nodes[1].node.get_and_clear_pending_events().len(), 0);
	} else {
		// Confirm the timeout tx and check that we fail the HTLC backwards
		header = BlockHeader { version: 0x20000000, prev_blockhash: header.bitcoin_hash(), merkle_root: Default::default(), time: 42, bits: 42, nonce: 42 };
		nodes[1].block_notifier.block_connected_checked(&header, CHAN_CONFIRM_DEPTH + ANTI_REORG_DELAY, &vec![], &[0; 0]);
		expect_pending_htlcs_forwardable!(nodes[1]);
	}

	check_added_monitors!(nodes[1], 1);
	// Which should result in an immediate claim/fail of the HTLC:
	let htlc_updates = get_htlc_update_msgs!(nodes[1], nodes[0].node.get_our_node_id());
	if claim {
		assert_eq!(htlc_updates.update_fulfill_htlcs.len(), 1);
		nodes[0].node.handle_update_fulfill_htlc(&nodes[1].node.get_our_node_id(), &htlc_updates.update_fulfill_htlcs[0]);
	} else {
		assert_eq!(htlc_updates.update_fail_htlcs.len(), 1);
		nodes[0].node.handle_update_fail_htlc(&nodes[1].node.get_our_node_id(), &htlc_updates.update_fail_htlcs[0]);
	}
	commitment_signed_dance!(nodes[0], nodes[1], htlc_updates.commitment_signed, false, true);
	if claim {
		expect_payment_sent!(nodes[0], our_payment_preimage);
	} else {
		let events = nodes[0].node.get_and_clear_pending_msg_events();
		assert_eq!(events.len(), 1);
		if let MessageSendEvent::PaymentFailureNetworkUpdate { update: HTLCFailChannelUpdate::ChannelClosed { ref is_permanent, .. } } = events[0] {
			assert!(is_permanent);
		} else { panic!("Unexpected event!"); }
		expect_payment_failed!(nodes[0], our_payment_hash, false);
	}
}

#[test]
fn test_onchain_htlc_claim_reorg_local_commitment() {
	do_test_onchain_htlc_reorg(true, true);
}
#[test]
fn test_onchain_htlc_timeout_delay_local_commitment() {
	do_test_onchain_htlc_reorg(true, false);
}
#[test]
fn test_onchain_htlc_claim_reorg_remote_commitment() {
	do_test_onchain_htlc_reorg(false, true);
}
#[test]
fn test_onchain_htlc_timeout_delay_remote_commitment() {
	do_test_onchain_htlc_reorg(false, false);
}
