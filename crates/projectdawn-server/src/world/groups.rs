//! Track 6 sub-task 5 — server-authoritative group state.
//!
//! Groups are ephemeral (in-memory only). On any member's disconnect
//! the group either continues with the remaining members or dissolves
//! if only one (or zero) members remain. No DB persistence in Track
//! 6; persistent guild state is a later track.
//!
//! Invite flow:
//!   1. Inviter sends `ClientWorldMsg::GroupInvite { name }`.
//!   2. Server resolves `name` → invitee char_id, stores a pending
//!      invite mapping invitee → inviter's group (or a fresh group
//!      if inviter is solo), forwards `ServerWorldMsg::GroupInvited
//!      { from_id, from_name }` to the invitee.
//!   3. Invitee sends `ClientWorldMsg::GroupAcceptInvite { from }`.
//!   4. Server validates the pending invite, adds invitee to the
//!      group, fans `GroupRoster` to every member.
//!
//! XP split:
//!   On enemy kill credit, if the killer is grouped, divide
//!   `mob.xp × (1.0 + GROUP_BONUS)` evenly among online members.
//!   Solo killer: full XP, no bonus.

use renet::ClientId;
use std::collections::HashMap;

/// Group XP bonus multiplier. 0.20 = 20% extra XP shared across the
/// group on kill, matching `autoloads/group_manager.gd`'s legacy
/// constant.
pub const GROUP_XP_BONUS: f32 = 0.20;

pub type GroupId = u64;

/// How a group distributes loot from kills its members are credited
/// with. Round Robin (default) rotates item-loot turns per corpse and
/// auto-splits coin among nearby members; Free-for-all lets any member
/// take any item (master-looter style) and gives all coin to the looter.
/// See `docs/design/group_loot_and_coin.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LootMode {
    #[default]
    RoundRobin,
    FreeForAll,
}

impl LootMode {
    /// Wire encoding for the group roster. 0 = Round Robin, 1 = FFA.
    pub fn to_u8(self) -> u8 {
        match self {
            LootMode::RoundRobin => 0,
            LootMode::FreeForAll => 1,
        }
    }

    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => LootMode::FreeForAll,
            _ => LootMode::RoundRobin,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Group {
    pub id: GroupId,
    pub leader: ClientId,
    pub members: Vec<ClientId>,
    /// Loot distribution rule for this group. Leader-set; defaults to
    /// Round Robin.
    pub loot_mode: LootMode,
    /// Round-robin pointer: index into `members` for the next corpse
    /// assignment. Advanced as corpses are assigned (Layer 3).
    pub loot_turn: usize,
}

#[derive(Debug, Default)]
pub struct GroupManager {
    next_id: GroupId,
    pub groups: HashMap<GroupId, Group>,
    /// char_id → group_id reverse index.
    pub member_to_group: HashMap<ClientId, GroupId>,
    /// invitee char_id → (inviter char_id, group_id). The invitee
    /// confirms by sending GroupAcceptInvite{from: inviter}; matching
    /// is by the inviter id stored here, not the group, so an
    /// inviter who left their group between sending and receiving
    /// the accept doesn't corrupt state.
    pub pending_invites: HashMap<ClientId, (ClientId, GroupId)>,
}

impl GroupManager {
    pub fn new() -> Self {
        Self { next_id: 1, ..Default::default() }
    }

    /// Create or fetch the inviter's group. If the inviter is solo,
    /// makes a new group with them as leader.
    pub fn group_for_or_new(&mut self, inviter: ClientId) -> GroupId {
        if let Some(&gid) = self.member_to_group.get(&inviter) {
            return gid;
        }
        let gid = self.next_id;
        self.next_id += 1;
        self.groups.insert(gid, Group {
            id: gid,
            leader: inviter,
            members: vec![inviter],
            loot_mode: LootMode::default(),
            loot_turn: 0,
        });
        self.member_to_group.insert(inviter, gid);
        gid
    }

    /// Record a pending invite. Returns the group_id the invitee will
    /// join if they accept.
    pub fn record_invite(&mut self, inviter: ClientId, invitee: ClientId) -> GroupId {
        let gid = self.group_for_or_new(inviter);
        self.pending_invites.insert(invitee, (inviter, gid));
        gid
    }

    /// Process an accept. Returns the GroupId the invitee joined +
    /// the full new roster (for fan-out) on success. Returns None if
    /// no pending invite matched.
    pub fn accept(&mut self, invitee: ClientId, from: ClientId) -> Option<GroupId> {
        let (recorded_inviter, gid) = self.pending_invites.remove(&invitee)?;
        if recorded_inviter != from {
            // Stale or wrong inviter — drop.
            return None;
        }
        // Invitee already in a group? Reject — they need to /leave
        // first. (Could auto-leave but that's a design choice.)
        if self.member_to_group.contains_key(&invitee) {
            return None;
        }
        let group = self.groups.get_mut(&gid)?;
        group.members.push(invitee);
        self.member_to_group.insert(invitee, gid);
        Some(gid)
    }

    /// Remove a member from their group. If they're the leader and
    /// others remain, promote the next member. If only they were in
    /// the group (or none after removal), dissolve. Returns
    /// (GroupId, surviving_members, dissolved_flag). On dissolve the
    /// `surviving_members` Vec is the list of clients who need a
    /// "group dissolved" notification (0 or 1 entries). On non-dissolve
    /// it's the new roster.
    pub fn leave(&mut self, member: ClientId) -> Option<(GroupId, Vec<ClientId>, bool)> {
        let gid = self.member_to_group.remove(&member)?;
        self.pending_invites.remove(&member);
        let group = self.groups.get_mut(&gid)?;
        group.members.retain(|&m| m != member);
        if group.members.is_empty() || group.members.len() == 1 {
            // Dissolve. Remaining solo member (if any) gets cleared
            // from the lookup map so a future invite makes a new
            // group with them as leader.
            let remaining = group.members.clone();
            for m in &remaining {
                self.member_to_group.remove(m);
            }
            self.groups.remove(&gid);
            return Some((gid, remaining, true));
        }
        if group.leader == member {
            group.leader = group.members[0];
        }
        Some((gid, group.members.clone(), false))
    }

    /// Look up the group containing this member, if any.
    pub fn group_of(&self, member: ClientId) -> Option<&Group> {
        let gid = self.member_to_group.get(&member)?;
        self.groups.get(gid)
    }

    /// True if both members belong to the same group. The ALLY heal/buff
    /// PvP gate uses this so group-mates can always support each other,
    /// even when both have `/pvp` flagged on.
    pub fn same_group(&self, a: ClientId, b: ClientId) -> bool {
        match (self.member_to_group.get(&a), self.member_to_group.get(&b)) {
            (Some(ga), Some(gb)) => ga == gb,
            _ => false,
        }
    }

    /// Pick the next Round-Robin loot recipient for a corpse owned by
    /// `killer`'s group, advancing the turn pointer past them. `eligible`
    /// filters candidates (the caller checks online + in range). Returns
    /// `None` when there is no item-turn restriction — the killer is solo
    /// / ungrouped, the group is Free-for-all, or nobody is eligible — and
    /// the caller then lets anyone with loot rights take items.
    pub fn next_loot_turn(
        &mut self,
        killer: ClientId,
        eligible: impl Fn(ClientId) -> bool,
    ) -> Option<ClientId> {
        let gid = *self.member_to_group.get(&killer)?;
        let group = self.groups.get_mut(&gid)?;
        if group.loot_mode != LootMode::RoundRobin {
            return None;
        }
        let n = group.members.len();
        if n == 0 {
            return None;
        }
        // Scan rotation order from the current pointer for the first
        // eligible member; advance the pointer just past them so the next
        // claimed corpse goes to someone else.
        for offset in 0..n {
            let idx = (group.loot_turn + offset) % n;
            let cand = group.members[idx];
            if eligible(cand) {
                group.loot_turn = (idx + 1) % n;
                return Some(cand);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build group {leader, members...} with the leader as member 0.
    fn grouped(gm: &mut GroupManager, leader: ClientId, members: &[ClientId]) {
        for &m in members {
            gm.record_invite(leader, m);
            let _ = gm.accept(m, leader);
        }
    }

    #[test]
    fn round_robin_rotates_through_members() {
        let mut gm = GroupManager::new();
        grouped(&mut gm, 1, &[2, 3]); // {1,2,3}, RR by default
        let seq = [
            gm.next_loot_turn(1, |_| true),
            gm.next_loot_turn(1, |_| true),
            gm.next_loot_turn(1, |_| true),
            gm.next_loot_turn(1, |_| true),
        ];
        assert_eq!(seq, [Some(1), Some(2), Some(3), Some(1)]);
    }

    #[test]
    fn ffa_group_has_no_turn() {
        let mut gm = GroupManager::new();
        grouped(&mut gm, 1, &[2]);
        let gid = *gm.member_to_group.get(&1).unwrap();
        gm.groups.get_mut(&gid).unwrap().loot_mode = LootMode::FreeForAll;
        assert_eq!(gm.next_loot_turn(1, |_| true), None);
    }

    #[test]
    fn solo_killer_has_no_turn() {
        let mut gm = GroupManager::new();
        assert_eq!(gm.next_loot_turn(42, |_| true), None);
    }

    #[test]
    fn skips_ineligible_members() {
        let mut gm = GroupManager::new();
        grouped(&mut gm, 1, &[2, 3]); // {1,2,3}
        // Only 3 is eligible (e.g. the only one in range); the turn lands
        // on 3 and the pointer advances past it.
        assert_eq!(gm.next_loot_turn(1, |c| c == 3), Some(3));
        assert_eq!(gm.next_loot_turn(1, |_| true), Some(1));
    }
}
