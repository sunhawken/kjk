//! The AI co-op seat: a policy that, each tick, reads the snapshot its seat
//! predicts and answers with the `Command` a human guest's keys and mouse
//! would make (`sim::input`: a seat's command is all the sim ever sees).
//! It never moves a body, sets a flag or advances a quest itself.
//!
//! Version 0 follows the first seat's body: turn to it, run when far, walk
//! when near, stand when close. It walks straight at it (no pathfinding
//! yet) and reports where it is, so a run can show it moved
//! (`KJK_AI_SEAT=1` turns it on for a joined seat).

use bevy::prelude::info;
use sim::moves::view_angles;
use sim::script::dialogue::{Pick, answer};
use sim::{Command, Seat, Snapshot, buttons};

/// Within this many metres of the leader the seat stands.
const STAND_WITHIN_M: f32 = 2.5;
/// Beyond this many metres it runs; between the two it walks.
const RUN_BEYOND_M: f32 = 6.0;
/// JA's client sends 64 instead of 127 on the stick when walking.
const WALK_AXIS: i8 = 64;
const RUN_AXIS: i8 = 127;
/// Ticks between answers while a conversation holds the seat: one line
/// skipped each time. It never picks a reply: the story's choices stay the
/// human player's.
const SKIP_EVERY_TICKS: u64 = 25;
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
pub fn gait(distance_m: f32) -> Gait {
    if distance_m <= STAND_WITHIN_M {
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

pub struct AiSeat {
    travelled_m: f32,
    last: Option<[f32; 3]>,
    reported: u64,
    /// The answer byte held for the sim (`Command::reply`).
    reply: u8,
    skipped: u64,
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
        }
    }

    /// The command for `tick` of `seat`, from the snapshot it predicts.
    pub fn decide(&mut self, snapshot: &Snapshot, seat: Seat, tick: u64) -> Command {
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
        let leader = snapshot.body(Seat(0)).map(|body| body.position);
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
        let distance = ((leader[0] - me[0]).powi(2) + (leader[1] - me[1]).powi(2)).sqrt();
        let mode = gait(distance);
        if tick >= self.reported + REPORT_TICKS {
            self.reported = tick;
            let mine = snapshot.body(seat);
            let theirs = snapshot.body(Seat(0));
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
        let angles = view_angles(0.0, yaw_toward(me, leader));
        let reply = self.reply;
        match mode {
            Gait::Stand => Command {
                angles,
                reply,
                ..Command::default()
            },
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
