//! State-machine property test of the portal input queue (`docs/api.md`,
//! `hud_input`): viewer replies are admitted within the item, byte and count
//! bounds, returned oldest first across the agent's portals, redelivered until
//! acknowledged, dropped at their TTL, and discarded with the portal on clear,
//! viewer dismiss or idle reclaim. A finite active poll defers only its
//! agent's idle degradation. Every step runs on an injected clock against a
//! plain model and compares the observable results.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use proptest::prelude::*;
use tze_hud_portal::{PortalError, PortalHub, PortalKey, PortalLimits, PortalStatus, Publish};

const AGENTS: [&str; 2] = ["a", "b"];
const PORTALS: [&str; 2] = ["p0", "p1"];

/// Small bounds so short runs reach every edge.
fn limits() -> PortalLimits {
    PortalLimits {
        input_items: 3,
        input_item_bytes: 6,
        input_total_bytes: 12,
        input_ttl_us: 5_000,
        degrade_after_us: 3_000,
        reclaim_after_us: 3_000,
        ..PortalLimits::default()
    }
}

#[derive(Clone, Debug)]
enum Op {
    /// Publish to (and so attach or touch) a portal.
    Publish {
        agent: usize,
        portal: usize,
    },
    /// The viewer types a reply into a portal's composer.
    Reply {
        agent: usize,
        portal: usize,
        text: String,
    },
    /// `hud_input`: ack some known ids (by index), take at most `max`.
    Poll {
        agent: usize,
        ack: Vec<usize>,
        max: Option<usize>,
    },
    /// A finite `wait_ms` poll begins, or a running one completes.
    BeginWait {
        agent: usize,
        remaining_us: u64,
    },
    EndWait {
        index: usize,
    },
    Advance {
        us: u64,
    },
    Sweep,
    Clear {
        agent: usize,
        portal: usize,
    },
    /// Viewer dismiss.
    Reclaim {
        agent: usize,
        portal: usize,
    },
    /// The owner's Clear of a whole agent.
    ReclaimAgent {
        agent: usize,
    },
}

fn op() -> impl Strategy<Value = Op> {
    let agent = 0..AGENTS.len();
    let portal = 0..PORTALS.len();
    prop_oneof![
        3 => (agent.clone(), portal.clone()).prop_map(|(agent, portal)| Op::Publish { agent, portal }),
        5 => (agent.clone(), portal.clone(), "[a-zé]{0,4}")
            .prop_map(|(agent, portal, text)| Op::Reply { agent, portal, text }),
        5 => (agent.clone(), prop::collection::vec(0..12usize, 0..3), prop::option::of(0..4usize))
            .prop_map(|(agent, ack, max)| Op::Poll { agent, ack, max }),
        1 => (agent.clone(), 0..8u64).prop_map(|(agent, n)| Op::BeginWait { agent, remaining_us: n * 500 }),
        1 => (0..4usize).prop_map(|index| Op::EndWait { index }),
        // Steps of 500 µs, a divisor of every limit, land exactly on TTL and idle deadlines.
        4 => (0..6u64).prop_map(|n| Op::Advance { us: n * 500 }),
        2 => Just(Op::Sweep),
        1 => (agent.clone(), portal.clone()).prop_map(|(agent, portal)| Op::Clear { agent, portal }),
        1 => (agent.clone(), portal.clone()).prop_map(|(agent, portal)| Op::Reclaim { agent, portal }),
        1 => agent.prop_map(|agent| Op::ReclaimAgent { agent }),
    ]
}

#[derive(Clone, Debug)]
struct Item {
    id: String,
    text: String,
    at_us: u64,
    expires_us: u64,
}

#[derive(Clone, Debug, Default)]
struct Live {
    inputs: Vec<Item>,
    last_seen_us: u64,
    degraded_since_us: Option<u64>,
}

#[derive(Clone, Debug)]
enum Slot {
    Live(Live),
    /// Reclaimed by the runtime: gone, but `clear` is still an ok no-op.
    Tombstone,
}

#[derive(Default)]
struct Model {
    now_us: u64,
    portals: BTreeMap<(String, String), Slot>,
    next_input: u64,
    /// Every id ever issued, for acks (including other agents' and stale ids).
    issued: Vec<String>,
    /// Finite waits: (agent, request, expires_us, still running).
    waits: Vec<(String, u64, u64, Arc<AtomicBool>)>,
}

impl Model {
    fn live(&mut self, key: &(String, String)) -> Option<&mut Live> {
        match self.portals.get_mut(key) {
            Some(Slot::Live(live)) => Some(live),
            _ => None,
        }
    }

    fn touch_agent(&mut self, agent: &str) {
        let now = self.now_us;
        for ((owner, _), slot) in self.portals.iter_mut() {
            if let (true, Slot::Live(live)) = (owner == agent, slot) {
                live.last_seen_us = now;
                live.degraded_since_us = None;
            }
        }
    }

    fn reply(&mut self, key: &(String, String), text: &str) -> Result<String, PortalError> {
        let l = limits();
        let now = self.now_us;
        let live = self.live(key).ok_or(PortalError::NotHeld)?;
        if text.len() > l.input_item_bytes {
            return Err(PortalError::TooLarge {
                limit: l.input_item_bytes,
            });
        }
        let bytes: usize = live.inputs.iter().map(|i| i.text.len()).sum();
        if live.inputs.len() >= l.input_items || bytes + text.len() > l.input_total_bytes {
            return Err(PortalError::QueueFull);
        }
        self.next_input += 1;
        let id = format!("i{}", self.next_input);
        self.live(key).expect("live above").inputs.push(Item {
            id: id.clone(),
            text: text.to_owned(),
            at_us: now,
            expires_us: now + l.input_ttl_us,
        });
        Ok(id)
    }

    /// Acked and expired items go first; the rest come back oldest first
    /// (portal order breaks ties), delivered or not, until acknowledged.
    fn poll(
        &mut self,
        agent: &str,
        ack: &[String],
        max: Option<usize>,
    ) -> (Vec<(String, String, String)>, usize) {
        self.touch_agent(agent);
        let now = self.now_us;
        let mut queued = Vec::new();
        for ((owner, id), slot) in self.portals.iter_mut() {
            let Slot::Live(live) = slot else { continue };
            if owner != agent {
                continue;
            }
            live.inputs
                .retain(|i| !ack.contains(&i.id) && i.expires_us > now);
            queued.extend(
                live.inputs
                    .iter()
                    .map(|i| (i.at_us, id.clone(), i.id.clone(), i.text.clone())),
            );
        }
        queued.sort_by_key(|(at, ..)| *at);
        let take = max.unwrap_or(usize::MAX).min(queued.len());
        let remaining = queued.len() - take;
        let items = queued
            .into_iter()
            .take(take)
            .map(|(_, portal, id, text)| (portal, id, text))
            .collect();
        (items, remaining)
    }

    fn sweep(&mut self) {
        let l = limits();
        let now = self.now_us;
        self.waits.retain(|(_, _, expires, active)| {
            active.load(std::sync::atomic::Ordering::Acquire) && *expires > now
        });
        let waiting: Vec<String> = self.waits.iter().map(|w| w.0.clone()).collect();
        for ((owner, _), slot) in self.portals.iter_mut() {
            let Slot::Live(live) = slot else { continue };
            live.inputs.retain(|i| i.expires_us > now);
            match live.degraded_since_us {
                None if !waiting.contains(owner)
                    && now >= live.last_seen_us + l.degrade_after_us =>
                {
                    live.degraded_since_us = Some(now);
                }
                Some(since) if now >= since + l.reclaim_after_us => *slot = Slot::Tombstone,
                _ => {}
            }
        }
    }
}

fn key(agent: usize, portal: usize) -> (String, String) {
    (AGENTS[agent].to_owned(), PORTALS[portal].to_owned())
}

fn portal_key(k: &(String, String)) -> PortalKey {
    PortalKey::new(k.0.as_str(), k.1.as_str())
}

/// What `hud_surfaces` shows per agent: (portal id, pending replies, degraded).
fn listed(hub: &PortalHub, agent: &str) -> Vec<(String, usize, bool)> {
    hub.list(agent)
        .into_iter()
        .map(|p| (p.id, p.pending_input, p.status == PortalStatus::Degraded))
        .collect()
}

fn model_listed(model: &Model, agent: &str) -> Vec<(String, usize, bool)> {
    model
        .portals
        .iter()
        .filter_map(|((owner, id), slot)| match slot {
            Slot::Live(live) if owner == agent => Some((
                id.clone(),
                live.inputs.len(),
                live.degraded_since_us.is_some(),
            )),
            _ => None,
        })
        .collect()
}

fn run(ops: Vec<Op>) -> Result<(), TestCaseError> {
    let mut hub = PortalHub::new(limits());
    let mut model = Model::default();
    for (step, op) in ops.into_iter().enumerate() {
        let now = model.now_us;
        match &op {
            Op::Publish { agent, portal } => {
                let k = key(*agent, *portal);
                hub.publish(&portal_key(&k), Publish::default(), now)
                    .map_err(|e| TestCaseError::fail(format!("step {step}: publish {e:?}")))?;
                if model.live(&k).is_none() {
                    model.portals.insert(
                        k.clone(),
                        Slot::Live(Live {
                            last_seen_us: now,
                            ..Live::default()
                        }),
                    );
                }
                let live = model.live(&k).expect("attached");
                live.last_seen_us = now;
                live.degraded_since_us = None;
            }
            Op::Reply {
                agent,
                portal,
                text,
            } => {
                let k = key(*agent, *portal);
                let got = hub.submit_reply(&portal_key(&k), text.clone(), now);
                let want = model.reply(&k, text);
                prop_assert_eq!(&got, &want, "step {}: {:?}", step, op);
                if let Ok(id) = want {
                    model.issued.push(id);
                }
            }
            Op::Poll { agent, ack, max } => {
                let agent = AGENTS[*agent];
                let ack: Vec<String> = ack
                    .iter()
                    .filter_map(|i| model.issued.get(*i).cloned())
                    .chain(std::iter::once("unknown".to_owned()))
                    .collect();
                let got = hub.poll_input(agent, &ack, *max, now);
                let (items, remaining) = model.poll(agent, &ack, *max);
                let got_items: Vec<_> = got
                    .items
                    .into_iter()
                    .map(|i| (i.portal, i.id, i.text))
                    .collect();
                prop_assert_eq!(got_items, items, "step {}: {:?}", step, op);
                prop_assert_eq!(got.remaining, remaining, "step {}: {:?}", step, op);
            }
            Op::BeginWait {
                agent,
                remaining_us,
            } => {
                let agent = AGENTS[*agent];
                let active = Arc::new(AtomicBool::new(true));
                let request = hub
                    .begin_input_poll(agent, *remaining_us, now, Arc::clone(&active))
                    .map_err(|e| TestCaseError::fail(format!("step {step}: wait {e:?}")))?;
                prop_assert_eq!(request.is_some(), *remaining_us > 0, "step {}", step);
                if let Some(request) = request {
                    model
                        .waits
                        .push((agent.to_owned(), request, now + remaining_us, active));
                }
            }
            Op::EndWait { index } => {
                if !model.waits.is_empty() {
                    let (agent, request, _, active) = model.waits.remove(index % model.waits.len());
                    active.store(false, std::sync::atomic::Ordering::Release);
                    hub.end_input_poll(&agent, request);
                }
            }
            Op::Advance { us } => model.now_us += us,
            Op::Sweep => {
                hub.sweep(now);
                model.sweep();
            }
            Op::Clear { agent, portal } => {
                let k = key(*agent, *portal);
                let got = hub.clear(&portal_key(&k));
                let want = model
                    .portals
                    .remove(&k)
                    .map(drop)
                    .ok_or(PortalError::NotHeld);
                prop_assert_eq!(got, want, "step {}: {:?}", step, op);
            }
            Op::Reclaim { agent, portal } => {
                let k = key(*agent, *portal);
                hub.reclaim(&portal_key(&k));
                if let Some(slot) = model.portals.get_mut(&k) {
                    *slot = Slot::Tombstone;
                }
            }
            Op::ReclaimAgent { agent } => {
                let agent = AGENTS[*agent];
                let mut want = 0;
                for ((owner, _), slot) in model.portals.iter_mut() {
                    if owner == agent && matches!(slot, Slot::Live(_)) {
                        *slot = Slot::Tombstone;
                        want += 1;
                    }
                }
                prop_assert_eq!(hub.reclaim_agent(agent), want, "step {}", step);
            }
        }
        for agent in AGENTS {
            prop_assert_eq!(
                listed(&hub, agent),
                model_listed(&model, agent),
                "step {}: {:?}",
                step,
                op
            );
        }
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 128, ..ProptestConfig::default() })]

    #[test]
    fn portal_input_queue_matches_its_model(ops in prop::collection::vec(op(), 1..80)) {
        run(ops)?;
    }
}

/// The rules the model encodes, pinned once by hand so a wrong model cannot
/// pass silently: an unacked reply comes back on every poll with its id; an
/// ack removes it; a queue full by count or by bytes refuses; the TTL drops it exactly
/// at expiry.
#[test]
fn the_model_encodes_redelivery_caps_and_ttl() {
    let mut hub = PortalHub::new(limits());
    let k = PortalKey::new("a", "p0");
    hub.publish(&k, Publish::default(), 0).unwrap();
    let id = hub.submit_reply(&k, "yes".into(), 0).unwrap();
    for _ in 0..2 {
        let batch = hub.poll_input("a", &[], None, 1);
        assert_eq!(
            batch
                .items
                .iter()
                .map(|i| i.id.as_str())
                .collect::<Vec<_>>(),
            [id.as_str()]
        );
    }
    assert!(
        hub.poll_input("a", std::slice::from_ref(&id), None, 2)
            .items
            .is_empty()
    );
    for text in ["a", "b", "c"] {
        hub.submit_reply(&k, text.into(), 3).unwrap();
    }
    assert_eq!(
        hub.submit_reply(&k, "d".into(), 3),
        Err(PortalError::QueueFull)
    );
    let expires = 3 + limits().input_ttl_us;
    assert_eq!(hub.poll_input("a", &[], None, expires - 1).items.len(), 3);
    assert!(hub.poll_input("a", &[], None, expires).items.is_empty());
    let k = PortalKey::new("a", "p1");
    hub.publish(&k, Publish::default(), 0).unwrap();
    for text in ["abcdef", "abcde"] {
        hub.submit_reply(&k, text.into(), 0).unwrap();
    }
    assert_eq!(
        hub.submit_reply(&k, "ab".into(), 0),
        Err(PortalError::QueueFull)
    );
}
