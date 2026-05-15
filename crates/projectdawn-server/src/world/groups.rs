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

#[derive(Debug, Clone)]
pub struct Group {
    pub id: GroupId,
    pub leader: ClientId,
    pub members: Vec<ClientId>,
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
}
