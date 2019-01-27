//
// Copyright 2018-2019 Tamas Blummer
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//
//!
//! # Download block filters
//!

use bitcoin::{
    BitcoinHash,
    network::{
        message::NetworkMessage,
        message_blockdata::{InvType, Inventory},
        message_filter::{
            CFHeaders, CFilter, GetCFHeaders, GetCFilters
        },
    },
    util::hash::Sha256dHash
};
use chaindb::SharedChainDB;
use p2p::{P2PControlSender, PeerMessage, PeerMessageReceiver, PeerMessageSender, PeerId, SERVICE_FILTERS};
use std::{
    sync::mpsc,
    thread,
    time::Duration,
};
use timeout::{ExpectedReply, SharedTimeout};
use error::SPVError;
use blockfilter::SCRIPT_FILTER;
use chaindb::StoredFilter;

pub struct FilterDownload {
    p2p: P2PControlSender,
    chaindb: SharedChainDB,
    timeout: SharedTimeout,
}

impl FilterDownload {
    pub fn new(chaindb: SharedChainDB, p2p: P2PControlSender, timeout: SharedTimeout) -> PeerMessageSender {
        let (sender, receiver) = mpsc::sync_channel(10);

        let mut filterdownload = FilterDownload { chaindb, p2p, timeout };

        thread::spawn(move || { filterdownload.run(receiver) });

        PeerMessageSender::new(sender)
    }

    fn run(&mut self, receiver: PeerMessageReceiver) {
        loop {
            while let Ok(msg) = receiver.recv_timeout(Duration::from_millis(1000)) {
                if let Err(e) = match msg {
                    PeerMessage::Connected(pid) => {
                        if self.is_serving_filters(pid) {
                            self.get_filter_headers(pid, SCRIPT_FILTER)
                        } else {
                            Ok(())
                        }
                    }
                    PeerMessage::Disconnected(_) => {
                        Ok(())
                    }
                    PeerMessage::Message(pid, msg) => {
                        match msg {
                            NetworkMessage::CFHeaders(headers) => if self.is_serving_filters(pid) { self.filter_headers(headers, pid) } else { Ok(()) },
                            NetworkMessage::CFilter(filter) => if self.is_serving_filters(pid) { self.filter(filter, pid) } else { Ok(()) },
                            NetworkMessage::Inv(inv) => if self.is_serving_filters(pid) { self.inv(inv, pid) } else { Ok(()) },
                            _ => { Ok(()) }
                        }
                    }
                } {
                    error!("Error processing filters: {}", e);
                }
            }
            self.timeout.lock().unwrap().check();
        }
    }

    fn is_serving_filters (&self, peer: PeerId) -> bool {
        if let Some(peer_version) = self.p2p.peer_version(peer) {
            return peer_version.services & SERVICE_FILTERS != 0;
        }
        false
    }

    fn get_filter_headers(&mut self, peer: PeerId, filter_type: u8) -> Result<(), SPVError> {
        if self.timeout.lock().unwrap().is_busy_with(peer, ExpectedReply::FilterHeader) {
            return Ok(());
        }
        let mut start_height = 0;
        let mut stop_hash = Sha256dHash::default();
        let chaindb = self.chaindb.read().unwrap();
        for header in chaindb.iter_trunk(0) {
            stop_hash = header.header.bitcoin_hash();
            if chaindb.get_block_filter(&stop_hash, filter_type).is_none () {
                start_height = header.height;
                break;
            }
        }
        let mut n = 0;
        for (i, id) in chaindb.iter_trunk(start_height).enumerate() {
            if i == 1000 {
                break;
            }
            n += 1;
            stop_hash = id.header.bitcoin_hash();
        }
        self.timeout.lock().unwrap().expect(peer, 1, ExpectedReply::FilterHeader);
        debug!("asking for {} {} filter headers after height {} peer={}", n, if filter_type == SCRIPT_FILTER {"script"} else {"coin"}, start_height, peer);
        self.p2p.send_network(peer, NetworkMessage::GetCFHeaders(GetCFHeaders{filter_type: SCRIPT_FILTER, start_height, stop_hash}));
        Ok(())
    }

    fn filter_headers(&mut self, headers: CFHeaders, peer: PeerId) -> Result<(), SPVError> {

        let previous_block_pos = if headers.previous_filter == Sha256dHash::default() {
            Some(0)
        }
        else {
            let chaindb = self.chaindb.read().unwrap();
            if let Some(filter) = chaindb.get_filter(&headers.previous_filter) {
                chaindb.pos_on_trunk(&filter.block_id)
            }
            else {
                None
            }
        };
        if let Some(trunk_pos) = previous_block_pos {
            self.timeout.lock().unwrap().received(peer, 1, ExpectedReply::FilterHeader);

            let mut chaindb = self.chaindb.write().unwrap();
            let mut previous = headers.previous_filter;
            let id_pairs = chaindb.iter_trunk(trunk_pos).map(|h|h.bitcoin_hash()).zip(headers.filter_hashes.iter().cloned()).collect::<Vec<_>>();
            for (block_id, filter_hash) in  id_pairs {

                let filter = StoredFilter { block_id, previous, filter_hash, filter: None, filter_type: headers.filter_type };
                previous = filter.bitcoin_hash();

                debug!("store {} filter header {} peer={}", if filter.filter_type == SCRIPT_FILTER {"script"} else {"coin"}, filter.block_id, peer);
                chaindb.add_filter(filter)?;
            }
        }
        Ok(())
    }

    fn filter (&mut self, filter: CFilter, peer: PeerId) -> Result<(), SPVError> {
        Ok(())
    }

    fn inv(&mut self, v: Vec<Inventory>, peer: PeerId) -> Result<(), SPVError> {
        let mut ask_for_headers = false;
        for inventory in v {
            // only care for blocks
            if inventory.inv_type == InvType::Block {
                let chaindb = self.chaindb.read().unwrap();
                debug!("received inv for block {}", inventory.hash);
                if chaindb.get_block_filter(&inventory.hash, SCRIPT_FILTER).is_none() {
                    // ask for filter headers if observing a new block
                    ask_for_headers = true;
                }
            }
        }
        if ask_for_headers {
            self.get_filter_headers(peer, SCRIPT_FILTER)?;
        }
        Ok(())
    }
}