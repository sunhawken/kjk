//! The AI co-op seat: a policy that, each tick, reads the snapshot its seat
//! predicts and answers with the `Command` a human guest's keys and mouse
//! would make (`sim::input`: a seat's command is all the sim ever sees).
//! It never moves a body, sets a flag or advances a quest itself.
//!
//! Version 0 follows the first seat's body: it stands when close, and else
//! walks the navigation mesh's route to it (`sim::walk::Walker`, the walker
//! the playthrough harness uses), replanned when the leader moves. It
//! reports where it is, so a run can show it moved (`KJK_AI_SEAT=1` turns it
//! on for a joined seat).

use std::collections::{BTreeMap, BTreeSet};

use bevy::prelude::info;
use sim::action::Action;
use sim::module::{ObjectKind, Opens};
use sim::motion::{SOLE_Z, UNIT};
use sim::moves::view_angles;
use sim::script::dialogue::{Pick, answer};
use sim::walk::Walker;
use sim::{Command, Seat, SimId, Snapshot, buttons};
use sim::{Kind, Level, ObjectState, TickRate};

/// Within this many metres of the leader the seat stands.
const STAND_WITHIN_M: f32 = 2.5;
/// A creature fighting a party member within this many metres is engaged.
const ENGAGE_M: f32 = 18.0;
/// How close (metres) a seat with a gun stands to its enemy, and with a
/// blade or fists.
const RANGED_STANDOFF_M: f32 = 7.0;
const MELEE_STANDOFF_M: f32 = 1.8;
/// Beyond this many metres it runs; between the two it walks.
const RUN_BEYOND_M: f32 = 6.0;
/// JA's client sends 64 instead of 127 on the stick when walking.
const WALK_AXIS: i8 = 64;
const RUN_AXIS: i8 = 127;
/// Ticks between answers while a conversation holds the seat: one line
/// skipped each time. It never picks a reply: the story's choices stay the
/// human player's.
const SKIP_EVERY_TICKS: u64 = 25;
/// The leader must move this far (metres) before the route is planned again.
const REPLAN_M: f32 = 3.0;
/// A closed door this close (metres, across the floor) gets the use key.
const DOOR_REACH_M: f32 = 2.2;
/// The use key is pressed for this many of every `USE_PERIOD_TICKS`.
const USE_HELD_TICKS: u64 = 4;
const USE_PERIOD_TICKS: u64 = 20;
/// Ticks between the actions asked of one door (an unlock takes a while).
const DOOR_ACTION_TICKS: u64 = 60;
/// Ticks between position reports in the log.
const REPORT_TICKS: u64 = 50;

/// What the seat did this tick, apart from the command.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Gait {
    Stand,
    Walk,
    Run,
}

/// The choice for one tick from the metres to the leader: the gait.
pub fn gait(distance_m: f32, standoff_m: f32) -> Gait {
    if distance_m <= standoff_m {
        Gait::Stand
    } else if distance_m <= RUN_BEYOND_M {
        Gait::Walk
    } else {
        Gait::Run
    }
}

/// The yaw, degrees from +X (90 is +Y), that faces `to` from `from`.
pub fn yaw_toward(from: [f32; 3], to: [f32; 3]) -> f32 {
    (to[1] - from[1]).atan2(to[0] - from[0]).to_degrees()
}

/// The nearest living creature within `ENGAGE_M` of `me` that is fighting one
/// of the `party` bodies: (its place, the metres to it, its hundredths of
/// HP). Only those already fighting the party are enemies, so a neutral is
/// never attacked.
fn nearest_enemy(
    remote: &Snapshot,
    party: [Option<SimId>; 2],
    me: [f32; 3],
) -> Option<([f32; 3], f32, i32)> {
    remote
        .entities
        .iter()
        .filter_map(|entity| {
            let Kind::Object(ObjectKind::Creature, _) = entity.kind else {
                return None;
            };
            let fighter = entity.fighter.filter(|fighter| fighter.alive())?;
            let target = fighter.target?;
            if !party.contains(&Some(target)) {
                return None;
            }
            let d = ((entity.position[0] - me[0]).powi(2) + (entity.position[1] - me[1]).powi(2))
                .sqrt();
            (d <= ENGAGE_M).then_some((entity.position, d, fighter.hp))
        })
        .min_by(|a, b| a.1.total_cmp(&b.1))
}

pub struct AiSeat {
    travelled_m: f32,
    last: Option<[f32; 3]>,
    reported: u64,
    /// The answer byte held for the sim (`Command::reply`).
    reply: u8,
    skipped: u64,
    /// The route to the leader, planned through the navigation mesh, and
    /// where the leader stood when it was.
    walker: Option<Walker>,
    planned_at: [f32; 3],
    explained: u64,
    fought: u64,
    /// Doors the story opens when used though they start locked (they
    /// have an `OnFailToOpen` script): planned through, as the playthrough
    /// harness does; for the level named here.
    tried: (String, BTreeSet<u32>),
    /// Actions this tick asks of the host (unlock or open a door), taken by
    /// the caller after `decide` and sent beside the command.
    pending: Vec<Action>,
    door_acted: BTreeMap<u32, u64>,
}

impl Default for AiSeat {
    fn default() -> Self {
        Self::new()
    }
}

impl AiSeat {
    pub fn new() -> Self {
        AiSeat {
            travelled_m: 0.0,
            last: None,
            reported: 0,
            reply: 0,
            skipped: 0,
            walker: None,
            planned_at: [0.0; 3],
            explained: 0,
            fought: 0,
            tried: (String::new(), BTreeSet::new()),
            pending: Vec::new(),
            door_acted: BTreeMap::new(),
        }
    }

    /// The actions the last `decide` asks to send beside its command.
    pub fn take_actions(&mut self) -> Vec<Action> {
        std::mem::take(&mut self.pending)
    }

    /// The command for `tick` of `seat`. `snapshot` is the one this client
    /// predicts (its own seat's body, ahead of the host); `remote` is the
    /// host's latest, where the other seats really are (a prediction leaves
    /// them standing, since it has none of their commands).
    pub fn decide(
        &mut self,
        level: &Level,
        snapshot: &Snapshot,
        remote: &Snapshot,
        seat: Seat,
        tick: u64,
    ) -> Command {
        // A conversation holds the world until its lines are passed.
        if snapshot.talk_for(seat).is_some() {
            if tick >= self.skipped + SKIP_EVERY_TICKS {
                self.skipped = tick;
                self.reply = answer(self.reply, Pick::Skip);
                info!(target: "ai_seat", "tick {tick} in a conversation: skipping a line");
            }
            return Command {
                reply: self.reply,
                ..Command::default()
            };
        }
        let me = snapshot.body(seat).map(|body| body.position);
        let leader = remote.body(Seat(0)).map(|body| body.position);
        let (Some(me), Some(leader)) = (me, leader) else {
            // No body (it watches) or no one to follow: stand.
            return Command::default();
        };
        if let Some(last) = self.last {
            let step = ((me[0] - last[0]).powi(2) + (me[1] - last[1]).powi(2)).sqrt();
            // A jump of metres in one frame is a load or a transfer, not
            // walking.
            if step < 2.0 {
                self.travelled_m += step;
            }
        }
        self.last = Some(me);
        // Fight what is fighting the party, else follow the leader.
        let mine = snapshot.body(seat);
        let enemy = nearest_enemy(
            remote,
            [
                mine.map(|body| body.id),
                remote.body(Seat(0)).map(|body| body.id),
            ],
            me,
        );
        let ranged = mine
            .and_then(|body| body.sheet)
            .is_none_or(|sheet| sheet.main_weapon().delivery.ranged());
        let (goal, standoff) = match enemy {
            Some((at, _, _)) => (
                at,
                if ranged {
                    RANGED_STANDOFF_M
                } else {
                    MELEE_STANDOFF_M
                },
            ),
            None => (leader, STAND_WITHIN_M),
        };
        let distance = ((goal[0] - me[0]).powi(2) + (goal[1] - me[1]).powi(2)).sqrt();
        let mode = gait(distance, standoff);
        if let Some((_, d, hp)) = enemy
            && tick >= self.fought + REPORT_TICKS
        {
            self.fought = tick;
            let fighter = mine.and_then(|body| body.fighter);
            if let Some(f) = fighter {
                let events: Vec<String> = f
                    .events
                    .iter()
                    .flatten()
                    .take(6)
                    .map(|event| format!("{event:?}"))
                    .collect();
                info!(target: "ai_seat", "tick {tick} my events {events:?} next {} saber {:?} shooting {:?} command_seen {:?}", f.next_event, f.saber, f.shooting, f.last_command.buttons);
            }
            info!(
                target: "ai_seat",
                "tick {tick} engaging an enemy {d:.1} m away (hp {:.1}), {mode:?}; me hp {:?} hand {:?} weapon_wait {:?} ranged {ranged} sheet {} yaw {:.0}",
                f64::from(hp) / 100.0,
                fighter.map(|f| f.hp),
                fighter.map(|f| f.hand),
                fighter.map(|f| f.weapon_wait_ms),
                mine.is_some_and(|body| body.sheet.is_some()),
                yaw_toward(me, goal)
            );
        }
        if tick >= self.reported + REPORT_TICKS {
            self.reported = tick;
            let who: Vec<String> = remote
                .entities
                .iter()
                .filter(|e| {
                    matches!(
                        e.kind,
                        Kind::Avatar(_) | Kind::Object(ObjectKind::Creature, _)
                    )
                })
                .take(8)
                .map(|e| {
                    format!(
                        "{:?}:{:?}@({:.1},{:.1})",
                        e.id.0, e.kind, e.position[0], e.position[1]
                    )
                })
                .collect();
            let talks: Vec<String> = remote
                .talk
                .conversations
                .iter()
                .map(|c| {
                    format!(
                        "{} entry {} replies {} party {} pc {:?} speaker {:?}",
                        c.dialog,
                        c.entry,
                        c.replies.len(),
                        c.party,
                        c.pc,
                        c.speaker
                    )
                })
                .collect();
            info!(target: "ai_seat", "tick {tick} host snapshot tick {} holds_world {} talks {talks:?}", remote.tick, remote.holds_world());
            info!(target: "ai_seat", "tick {tick} host bodies {who:?} leading {:?}", remote.leading);
            let crowd: Vec<String> = remote
                .entities
                .iter()
                .filter_map(|e| {
                    let Kind::Object(ObjectKind::Creature, index) = e.kind else {
                        return None;
                    };
                    let f = e.fighter?;
                    let d = ((e.position[0] - me[0]).powi(2) + (e.position[1] - me[1]).powi(2)).sqrt();
                    (d < 25.0).then(|| format!(
                        "#{} idx{index} hp {} life {:?} tgt {:?} d {d:.1} at ({:.1},{:.1}) hand {} wait {}",
                        e.id.0, f.hp, f.life, f.target.map(|t| t.0), e.position[0], e.position[1], f.hand, f.weapon_wait_ms
                    ))
                })
                .collect();
            info!(target: "ai_seat", "tick {tick} crowd {crowd:?} (me #{:?} leader #{:?})", snapshot.body(seat).map(|b| b.id.0), remote.body(Seat(0)).map(|b| b.id.0));
            let mine = snapshot.body(seat);
            let theirs = remote.body(Seat(0));
            info!(
                target: "ai_seat",
                "tick {tick} seat {} body {:?} {:?} at ({:.2}, {:.2}, {:.2}) vel {:?} leader body {:?} ({:.2}, {:.2}) distance {distance:.2} m gait {mode:?} travelled {:.2} m world_held {} leading {:?}",
                seat.0,
                mine.map(|body| body.id),
                mine.map(|body| body.kind),
                me[0], me[1], me[2],
                mine.map(|body| body.velocity),
                theirs.map(|body| body.id),
                leader[0], leader[1], self.travelled_m,
                snapshot.holds_world(),
                snapshot.leading
            );
        }
        // The sim adds the body's `view_offset` (delta angles, set when a
        // script turned or placed it) to a command's angles, as JA does: to
        // look at a bearing the command carries it less that offset.
        let offset = mine
            .and_then(|body| body.player)
            .map_or([0i16; 3], |player| player.view_offset);
        let rebase = |angles: [i16; 3]| {
            [
                angles[0].wrapping_sub(offset[0]),
                angles[1].wrapping_sub(offset[1]),
                angles[2].wrapping_sub(offset[2]),
            ]
        };
        let angles = rebase(view_angles(0.0, yaw_toward(me, goal)));
        let reply = self.reply;
        // Walk the navigation mesh's route to the leader when the body has
        // a motion state; else straight at it.
        if mode != Gait::Stand
            && let Some(player) = snapshot.body(seat).and_then(|body| body.player)
        {
            let leader_moved = ((goal[0] - self.planned_at[0]).powi(2)
                + (goal[1] - self.planned_at[1]).powi(2))
            .sqrt()
                > REPLAN_M;
            if self.walker.is_none() || leader_moved {
                let soles = (player.origin[2] + SOLE_Z) * UNIT;
                let floor = level
                    .nav()
                    .and_then(|nav| nav.height([goal[0], goal[1], soles], 50.0, 50.0))
                    .unwrap_or(goal[2]);
                self.walker = Some(Walker::new([
                    goal[0] / UNIT,
                    goal[1] / UNIT,
                    floor / UNIT - SOLE_Z,
                ]));
                self.planned_at = goal;
            }
            if self.tried.0 != level.name() {
                self.tried = (
                    level.name().to_string(),
                    (0u32..)
                        .zip(level.placed().iter())
                        .filter(|(_, object)| {
                            object.kind == ObjectKind::Door
                                && object
                                    .scripts
                                    .iter()
                                    .any(|(hook, _)| hook == "OnFailToOpen")
                        })
                        .map(|(index, _)| index)
                        .collect(),
                );
            }
            let tried = &self.tried.1;
            let doors: BTreeMap<u32, ObjectState> = snapshot
                .entities
                .iter()
                .filter_map(|entity| match (entity.kind, entity.object) {
                    (Kind::Object(_, index), Some(object)) => Some((index, object)),
                    _ => None,
                })
                .collect();
            let now_ms = TickRate::default().time_ms(tick);
            if let Some(walker) = self.walker.as_mut() {
                let mut command = walker.command_eager(level, &player, now_ms, &|index| {
                    let mut state = doors.get(&index).copied().unwrap_or(ObjectState::GONE);
                    let opens_at_all = level
                        .placed()
                        .get(index as usize)
                        .is_some_and(|object| object.opens != Opens::Never);
                    if tried.contains(&index) || opens_at_all {
                        state.lock = sim::lock::UNLOCKED;
                    }
                    state
                });
                command.reply = reply;
                // A closed door close by: press use on it, as a player does.
                let door_near = snapshot.entities.iter().any(|entity| {
                    matches!(entity.kind, Kind::Object(ObjectKind::Door, _))
                        && entity.object.is_some_and(|object| object.state == 0)
                        && ((entity.position[0] - me[0]).powi(2)
                            + (entity.position[1] - me[1]).powi(2))
                        .sqrt()
                            < DOOR_REACH_M
                });
                if door_near && tick % USE_PERIOD_TICKS < USE_HELD_TICKS {
                    command.buttons |= buttons::USE;
                }
                // The nearest closed door within reach: unlock it (Security)
                // if it is locked, else open it, as a player's click does.
                let closest = snapshot
                    .entities
                    .iter()
                    .filter_map(|entity| match (entity.kind, entity.object) {
                        (Kind::Object(ObjectKind::Door, index), Some(object))
                            if object.state == 0 =>
                        {
                            let d = ((entity.position[0] - me[0]).powi(2)
                                + (entity.position[1] - me[1]).powi(2))
                            .sqrt();
                            (d < DOOR_REACH_M).then_some((d, index, object))
                        }
                        _ => None,
                    })
                    .min_by(|a, b| a.0.total_cmp(&b.0));
                if let Some((_, index, object)) = closest {
                    let due = self
                        .door_acted
                        .get(&index)
                        .is_none_or(|last| tick >= last + DOOR_ACTION_TICKS);
                    if due {
                        self.door_acted.insert(index, tick);
                        let opens = level
                            .placed()
                            .get(index as usize)
                            .map_or(Opens::Never, |facts| facts.opens);
                        let action = if object.locked(opens) {
                            Action::Unlock {
                                object: index,
                                spike: None,
                            }
                        } else {
                            Action::Use {
                                object: index,
                                verb: sim::action::Verb::Open,
                            }
                        };
                        info!(target: "ai_seat", "tick {tick} door {index}: asking {action:?}");
                        self.pending.push(action);
                    }
                }
                if command.forward == 0
                    && command.right == 0
                    && tick >= self.explained + REPORT_TICKS
                {
                    self.explained = tick;
                    let why = sim::walk::why_none(level, player.origin, walker.goal);
                    info!(target: "ai_seat", "tick {tick} the walker stands still: {why} (from {:?} to {:?} units)", player.origin, walker.goal);
                    // Is a door the only thing in the way? Plan with every
                    // door open, and list the nearest doors as they stand.
                    let open = ObjectState {
                        state: 1,
                        ..ObjectState::default()
                    };
                    let all_open = sim::walk::route(level, player.origin, walker.goal, &|_| open);
                    info!(
                        target: "ai_seat",
                        "with every door open a route {} ({} corners)",
                        if all_open.is_some() { "EXISTS" } else { "does not exist" },
                        all_open.as_ref().map_or(0, Vec::len)
                    );
                    let mut near: Vec<(f32, u32, ObjectState)> = snapshot
                        .entities
                        .iter()
                        .filter_map(|entity| match (entity.kind, entity.object) {
                            (Kind::Object(ObjectKind::Door, index), Some(object)) => Some((
                                ((entity.position[0] - me[0]).powi(2)
                                    + (entity.position[1] - me[1]).powi(2))
                                .sqrt(),
                                index,
                                object,
                            )),
                            _ => None,
                        })
                        .collect();
                    near.sort_by(|a, b| a.0.total_cmp(&b.0));
                    for (distance, index, object) in near.iter().take(4) {
                        info!(
                            target: "ai_seat",
                            "door {index} {distance:.1} m: state {} lock {} tried {}",
                            object.state, object.lock, tried.contains(index)
                        );
                    }
                }
                return command;
            }
        }
        match mode {
            Gait::Stand => {
                if let Some((at, d, _)) = enemy {
                    // In range: face it and hold the attack. The pitch
                    // aims at its chest; positive looks down.
                    let pitch = -((at[2] + 0.8) - (me[2] + 0.6))
                        .atan2(d.max(0.1))
                        .to_degrees();
                    Command {
                        angles: rebase(view_angles(pitch.clamp(-30.0, 30.0), yaw_toward(me, at))),
                        buttons: buttons::ATTACK,
                        reply,
                        ..Command::default()
                    }
                } else {
                    Command {
                        angles,
                        reply,
                        ..Command::default()
                    }
                }
            }
            Gait::Walk => Command {
                angles,
                forward: WALK_AXIS,
                buttons: buttons::WALKING,
                reply,
                ..Command::default()
            },
            Gait::Run => Command {
                angles,
                forward: RUN_AXIS,
                reply,
                ..Command::default()
            },
        }
    }
}
