//! The AI as the party's leader (`KJK_AI_LEAD_PLAN=FILE`): it plays the
//! host's own seat (seat 0, the conversation's listener) through a quest
//! plan, in the order a route file gives it, while any other seat, a human
//! who joined, follows. The plan is the route format's verbs, the ones a
//! player does by hand:
//!
//! - `to TAG`: walk to the nearest object with the tag;
//! - `use TAG[@N]`, `exit TAG`: walk there and use it (a door is opened, one
//!   who speaks is talked to, a trigger is walked into);
//! - `talk`: wait for the conversation it started and see it through, its
//!   replies chosen by the partner's policy (`say` phrases first, then the
//!   light-side words, `ai_seat::pick_reply`);
//! - `say PHRASE`: a phrase to choose a reply by, kept for the rest of the
//!   plan;
//! - `fight [SECONDS]`: fight what fights the party (the seat's own combat).
//!
//! Anything else in a route (`party`, `save`, `module`, `pick`, `give`,
//! `if`...`end`) is listed in the log as skipped: the plan is a guide the
//! leader follows, not the playthrough harness. A step that does not finish
//! in `STEP_LIMIT_S` of play is given up on and logged, so a plan never
//! stalls the game for good. Everything it decides is in the run's log
//! (`ai_leader` target) and `facts` (`state`).

use sim::action::Action;
use sim::module::ObjectKind;
use sim::{Command, Level, Seat, Snapshot};
use bevy::prelude::{error, info};

use crate::ai_seat::{AiSeat, Delegate};

/// The leader's seat: the host's.
const SEAT: Seat = Seat(0);
/// Play time before a step is given up on.
const STEP_LIMIT_S: u64 = 40;

/// A step whose body has moved less than `STUCK_M` in `STUCK_S` seconds,
/// with no talk or press to wait on, is given up on at once.
const STUCK_S: u64 = 10;
const STUCK_M: f32 = 0.5;
/// Ticks per second of play (`sim::TICK_HZ`).
const HZ: u64 = 125;
/// A use is pressed again after this many ticks without an answer.
const PRESS_EVERY: u64 = HZ * 2;
/// Presses before a use counts as done (a door that opened, a talk that
/// never came) and the plan goes on.
const PRESSES: u32 = 6;

/// The fight step's default length (s).
const FIGHT_S: u64 = 20;
/// A conversation over for this long (ticks) ends a `talk` step.
const TALK_OVER: u64 = HZ * 2;

/// One step of the plan.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    To(String),
    Use(String, usize),
    Exit(String),
    Talk,
    Say(String),
    Fight(u64),
    /// Make companion NPC_n (0-based) the party's leader, or the player again.
    Leader(Option<u8>),
    /// Security on a locked door or placeable.
    Unlock(String),
    /// Walk to a point (`at X Y`, as a route writes it).
    At(f32, f32),
    /// Wear what the party carries (`equip`, or one item by template).
    Equip(Option<String>),
    /// A verb the leader does not do (shown in the log).
    Skipped(String),
}

/// A plan from a route file's text: each line a verb, `#` a comment.
pub fn parse(text: &str) -> Vec<Step> {
    let mut steps = Vec::new();
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let (verb, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        let rest = rest.trim();
        let (tag, nth) = match rest.split_once('@') {
            Some((tag, n)) => (tag.trim().to_string(), n.trim().parse().unwrap_or(1usize)),
            None => (rest.to_string(), 1),
        };
        steps.push(match verb {
            "to" if !rest.is_empty() => Step::To(tag),
            "use" if !rest.is_empty() => Step::Use(tag, nth),
            "exit" if !rest.is_empty() => Step::Exit(tag),
            "talk" => Step::Talk,
            "at" => {
                let mut n = rest.split_whitespace().filter_map(|v| v.parse::<f32>().ok());
                match (n.next(), n.next()) {
                    (Some(x), Some(y)) => Step::At(x, y),
                    _ => Step::Skipped(line.to_string()),
                }
            }
            "equip" => Step::Equip(if rest.is_empty() { None } else { Some(rest.to_ascii_lowercase()) }),
            "leader" if rest == "player" => Step::Leader(None),
            "leader" => match rest.parse() {
                Ok(n) => Step::Leader(Some(n)),
                Err(_) => Step::Skipped(line.to_string()),
            },
            "unlock" if !rest.is_empty() => Step::Unlock(tag),
            "say" if !rest.is_empty() => Step::Say(rest.to_string()),
            "fight" => Step::Fight(rest.split_whitespace().next().and_then(|n| n.parse().ok()).unwrap_or(FIGHT_S)),
            _ => Step::Skipped(line.to_string()),
        });
    }
    steps
}

/// What a walk to use an object did this tick.
enum Used {
    /// The walk's command, in place of the seat's own.
    Steer(Command),
    /// Within reach: the use was asked of the host.
    Asked,
    /// Waiting for the last press to answer.
    Waiting,
    /// The walk finds no way (the body stands off the walkable mesh): the
    /// seat's own steering goes toward the object instead.
    Stuck,
}

/// The leader's state across ticks.
pub struct Leader {
    steps: Vec<Step>,
    at: usize,
    ai: AiSeat,
    /// The tick the current step began, and its use presses.
    began: u64,
    pressed: u32,
    last_press: u64,
    /// For a talk step: a conversation seen, and the tick it was last seen.
    talked: bool,
    talk_seen: u64,
    /// The module the step began in (a transition ends an exit).
    module: String,
    /// When a door on the way was last pressed.
    door_pressed: u64,
    /// A closed door the walk is heading to open, the way to the step's goal being shut.
    door_goal: Option<u32>,
    /// A stand short of an object no way leads to (index, feet, metres): the harness's `approach`.
    stand_alt: Option<(u32, [f32; 3])>,
    /// Where the body was and since when (a step's stall).
    stall: ([f32; 3], u64),
    /// The walk to the object under use, as a player's use key makes it.
    walk: Option<sim::target::Walk>,
    /// The end of the plan was logged.
    done_logged: bool,
    /// A party table was shown and is not yet closed.
    party_screen: bool,
    /// A conversation seen through by a use that the next `talk` claims.
    unclaimed: bool,
    /// What the host is asked to do this tick (`take_request`).
    request: Request,
    /// When a container's contents were first reached (0: not yet), what the
    /// party carried then, and when it was last asked for them.
    loot: u64,
    carried_before: usize,
    last_take: u64,
}

/// The use key's work for the host (`target_view`'s `Pinned` and `Acted`).
#[derive(Default, Debug)]
pub struct Request {
    /// The target to hold (`Some(None)` lets it go).
    pub pin: Option<Option<String>>,
    /// The use key pressed.
    pub act: bool,
    /// The walk to the target steers, not the leader's own command.
    pub defer: bool,
}

impl Leader {
    /// The leader of `KJK_AI_LEAD_PLAN`, when set and readable.
    pub fn from_env() -> Option<Leader> {
        let path = std::env::var_os("KJK_AI_LEAD_PLAN")?;
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) => {
                error!(target: "ai_leader", "cannot read the plan {path:?}: {error}");
                return None;
            }
        };
        let steps = parse(&text);
        info!(target: "ai_leader", "the AI leads: {} steps from {path:?}", steps.len());
        Some(Leader::new(steps))
    }

    pub fn new(steps: Vec<Step>) -> Leader {
        let mut ai = AiSeat::new();
        ai.allow_exits = true;
        ai.consume_says = true;
        // The leader answers its own conversations: the light-side policy
        // unless the environment says dark.
        if ai.delegate.is_none() {
            ai.delegate = Some(Delegate::Light);
        }
        Leader {
            steps,
            at: 0,
            ai,
            began: 0,
            pressed: 0,
            last_press: 0,
            talked: false,
            talk_seen: 0,
            module: String::new(),
            door_pressed: 0,
            door_goal: None,
            stand_alt: None,
            stall: ([0.0; 3], 0),
            walk: None,
            done_logged: false,
            party_screen: false,
            unclaimed: false,
            request: Request::default(),
            loot: 0,
            carried_before: 0,
            last_take: 0,
        }
    }

    /// The host's work for this tick, taken.
    pub fn take_request(&mut self) -> Request {
        std::mem::take(&mut self.request)
    }

    /// A script showed the party table (the leader closes it).
    pub fn party_screen_shown(&mut self) {
        self.party_screen = true;
    }

    /// The step under way and how many there are (for the facts).
    pub fn progress(&self) -> (usize, usize) {
        (self.at, self.steps.len())
    }

    pub fn finished(&self) -> bool {
        self.at >= self.steps.len()
    }

    fn next(&mut self, tick: u64, why: &str) {
        info!(target: "ai_leader", "tick {tick} step {} {:?} done ({why})", self.at + 1, self.steps.get(self.at));
        self.at += 1;
        self.began = tick;
        self.pressed = 0;
        self.last_press = 0;
        self.talked = false;
        self.loot = 0;
        self.walk = None;
        self.door_goal = None;
        self.stand_alt = None;
        self.request.pin = Some(None);
        self.ai.set_lead(None);
    }

    /// The nearest-first `nth` (from 1) object of the level with `tag`:
    /// its index and data, by distance from `from`.
    fn find<'a>(level: &'a Level, tag: &str, nth: usize, from: [f32; 3]) -> Option<(u32, &'a sim::module::ObjectData)> {
        let mut found: Vec<(u32, &sim::module::ObjectData)> = level
            .placed()
            .iter()
            .enumerate()
            .filter(|(_, object)| object.tag.eq_ignore_ascii_case(tag))
            .map(|(index, object)| (index as u32, &**object))
            .collect();
        found.sort_by(|a, b| flat(a.1.position, from).total_cmp(&flat(b.1.position, from)));
        found.into_iter().nth(nth.saturating_sub(1))
    }

    /// Walk to object `index` and use it as the player's use key does
    /// (`game::target_view`, over `sim::target`): the first thing it offers,
    /// from where the object is used (a door's action point), the walk
    /// steering until within reach, then the use asked of the host.
    fn use_object(&mut self, world: &sim::SimWorld, snapshot: &Snapshot, index: u32, unlock: bool, walk_only: bool, tick: u64, actions: &mut Vec<Action>) -> Option<Used> {
        use sim::target::{Spot, Stride, Walk};
        let level = world.level();
        let me = snapshot.body(SEAT)?;
        let ps = me.player?;
        let entity = sim::target::entity(snapshot, index)?;
        let instances = sim::Instances::in_snapshot(level, snapshot);
        let facts = instances.of_entity(entity)?;
        let state = entity.object.unwrap_or_default();
        // A door that stays locked after the first tries is picked with the
        // character's Security (the plan's own Open comes first: a lock's
        // refusal is often a scene the story wants played).
        let lock_picking = unlock || (self.pressed >= 2 && state.locked(facts.opens));
        let security = lock_picking.then_some(sim::target::Security { spike: None });
        let choices = sim::target::choices(facts.kind, facts, state, false, security, None);
        let choice = if lock_picking {
            let picked = choices.iter().find(|choice| matches!(choice, sim::target::Choice::Unlock { .. }));
            match picked {
                Some(choice) => *choice,
                None if unlock => return None,
                None => *choices.first()?,
            }
        } else if unlock {
            // Security on a locked door: nothing to do when it is not locked.
            *choices.iter().find(|choice| matches!(choice, sim::target::Choice::Unlock { .. }))?
        } else {
            *choices.first()?
        };
        let feet = sim::target::candidates(level, snapshot, Some(me.id))
            .into_iter()
            .find(|candidate| candidate.index == index)?
            .feet;
        let spot = Spot::in_snapshot(level, snapshot, (index, facts.kind), choice, feet, &ps);
        // Doors that open when used are ways, closed as they stand (opened on
        // the way, below), as the partner's own walker plans them.
        let doors = |number: u32| {
            let mut state = sim::target::entity(snapshot, number)
                .and_then(|entity| entity.object)
                .unwrap_or_default();
            if level
                .placed()
                .get(number as usize)
                .is_some_and(|object| {
                    object.kind == ObjectKind::Door
                        && (object.opens != sim::module::Opens::Never
                            // A locked door with a fail-to-open script is tried: the script may open it
                            // (the playthrough's `to_try`).
                            || object.scripts.iter().any(|(hook, _)| hook == "OnFailToOpen"))
                })
                && state.lock != sim::lock::LOCKED
            {
                state.lock = sim::lock::UNLOCKED;
            }
            state
        };
        // A closed door close by on the way: opened, as a player's click does.
        if tick >= self.door_pressed + 2 * HZ
            && let Some((door, _)) = snapshot
                .entities
                .iter()
                .filter_map(|entity| match (entity.kind, entity.object) {
                    (sim::Kind::Object(ObjectKind::Door, number), Some(state)) if number != index && state.state == 0 => {
                        let near = flat(entity.position, me.position);
                        (near < 3.5).then_some((number, near))
                    }
                    _ => None,
                })
                .min_by(|a, b| a.1.total_cmp(&b.1))
        {
            self.door_pressed = tick;
            info!(target: "ai_leader", "tick {tick} a closed door {door} on the way: opening it");
            actions.push(Action::Use { object: door, verb: sim::action::Verb::Open });
        }
        let none: Vec<bool> = Vec::new();
        let ways = sim::walk::Walled { doors: &doors, walled: &none };
        let now_ms = snapshot.time_ms;
        if tick % (HZ * 4) == 0 && self.pressed > 0 {
            info!(target: "ai_leader", "tick {tick} object {index}: state {:?}, choices {choices:?}, picked {choice:?}", state, );
        }
        if tick < self.last_press + PRESS_EVERY && self.pressed > 0 {
            return Some(Used::Waiting);
        }
        // No way leads to the object's own point (a footlocker's): a stand short
        // of it that a way leads to, nearest the body first, then round it, as
        // the playthrough's `approach` picks (kept while the walk lasts).
        let mut spot = spot;
        // Where the walk heads for `at`: the stand-off short of it toward the
        // body (`Walk::plan`), which a way must lead to.
        let stand = spot.stand_off(choice);
        let leads = |at: [f32; 3]| {
            let feet = sim::target::feet_at(ps.origin);
            let (dx, dy) = (feet[0] - at[0], feet[1] - at[1]);
            let length = (dx * dx + dy * dy).sqrt().max(1e-3);
            let g = sim::math::scale([at[0] + dx / length * stand, at[1] + dy / length * stand, at[2]], 1.0 / sim::motion::UNIT);
            sim::walk::route_ways(level, ps.origin, [g[0], g[1], g[2] - sim::motion::SOLE_Z], &ways).is_some()
        };
        match self.stand_alt {
            Some((kept, at)) if kept == index => spot.at = at,
            _ => {
                self.stand_alt = None;
                if !leads(spot.at) {
                    let feet = sim::target::feet_at(ps.origin);
                    let d = sim::math::flat_distance(feet, spot.at).max(f32::EPSILON);
                    let toward = [0.8f32, 1.6, 2.4].into_iter().filter(|step| *step < d).map(|step| {
                        [
                            spot.at[0] + (feet[0] - spot.at[0]) / d * step,
                            spot.at[1] + (feet[1] - spot.at[1]) / d * step,
                            spot.at[2],
                        ]
                    });
                    let round = [0.8f32, 1.2, 1.6, 2.4].into_iter().flat_map(|far| {
                        (0..8).map(move |k| {
                            let angle = k as f32 * std::f32::consts::FRAC_PI_4;
                            [spot.at[0] + far * angle.cos(), spot.at[1] + far * angle.sin(), spot.at[2]]
                        })
                    });
                    if let Some(at) = toward.chain(round).find(|at| leads(*at)) {
                        info!(target: "ai_leader", "tick {tick} no way to object {index}'s own point: a stand at {:?} instead", at);
                        self.stand_alt = Some((index, at));
                        spot.at = at;
                    }
                }
            }
        }
        if self.walk.is_none() {
            if Walk::near(&ps, spot) {
                if !walk_only {
                    info!(target: "ai_leader", "tick {tick} using object {index} {choice:?} from {:.2} m (within {:.2}, reach {:.2}) of its use point; feet {:?} spot {:?}", sim::math::flat_distance(sim::target::feet_at(ps.origin), spot.at), spot.within, spot.reach, sim::target::feet_at(ps.origin), spot.at);
                    actions.extend(sim::target::ask(choice, index, facts.kind));
                }
                return Some(Used::Asked);
            }
            self.walk = Some(Walk::new(level, &ways, &ps, index, spot, choice, now_ms));
        }
        let mut walk = self.walk?;
        match walk.stride(level, &ways, &ps, spot, now_ms) {
            Stride::Steer(command) => {
                self.walk = Some(walk);
                Some(Used::Steer(command))
            }
            Stride::Arrived => {
                self.walk = None;
                if !walk_only {
                    info!(target: "ai_leader", "tick {tick} arrived at object {index} {choice:?}: {:.2} m from its use point (within {:.2}, reach {:.2}); feet {:?} spot {:?}", sim::math::flat_distance(sim::target::feet_at(ps.origin), spot.at), spot.within, spot.reach, sim::target::feet_at(ps.origin), spot.at);
                    actions.extend(sim::target::ask(choice, index, facts.kind));
                }
                Some(Used::Asked)
            }
            Stride::Unreachable => {
                self.walk = None;
                let goal = sim::math::scale(spot.at, 1.0 / sim::motion::UNIT);
                info!(
                    target: "ai_leader",
                    "tick {tick} the walk to object {index} cannot reach it: from {:?} to {:?} ({}); feet {:?} spot {:?}",
                    ps.origin, goal, sim::walk::why_none(level, ps.origin, [goal[0], goal[1], goal[2] - sim::motion::SOLE_Z]), sim::target::feet_at(ps.origin), spot.at
                );
                // Which door, opened alone, would give a way (a diagnosis).
                let to = [goal[0], goal[1], ps.origin[2]];
                let all_open = |_: u32| sim::ObjectState { state: 1, ..Default::default() };
                info!(target: "ai_leader", "route with every door open: {}", sim::walk::route(level, ps.origin, to, &all_open).is_some());
                for probe in [[62.2f32, 90.0], [70.0, 85.0], [76.9, 84.0], [76.9, 79.0], [76.0, 76.0], [74.5, 74.5], [74.5, 72.9], [79.0, 70.0]] {
                    let at = [probe[0] / sim::motion::UNIT, probe[1] / sim::motion::UNIT, ps.origin[2]];
                    info!(target: "ai_leader", "route to {:?} (every door open): {}", probe, sim::walk::route(level, ps.origin, at, &all_open).is_some());
                }
                for entity in &snapshot.entities {
                    if let sim::Kind::Object(ObjectKind::Door, number) = entity.kind {
                        let only = |n: u32| {
                            if n == number {
                                sim::ObjectState { state: 1, ..Default::default() }
                            } else {
                                doors(n)
                            }
                        };
                        info!(target: "ai_leader", "route with door {number} open: {}", sim::walk::route(level, ps.origin, to, &only).is_some());
                    }
                }
                Some(Used::Stuck)
            }
        }
    }

    /// The seat's command this tick, and the actions it takes. `texts` are
    /// the reply texts of the conversation the seat is in.
    pub fn command(&mut self, world: &sim::SimWorld, snapshot: &Snapshot, texts: &[String], tick: u64) -> (Command, Vec<Action>) {
        let level = world.level();
        let mut actions = Vec::new();
        if self.began == 0 {
            self.began = tick;
            self.module = level.name().to_string();
            info!(target: "ai_leader", "tick {tick} leading from {} through {} steps", self.module, self.steps.len());
        }
        let me = snapshot.body(SEAT).map(|body| body.position);
        if tick % 250 == 0
            && let (Some(at), Some(step)) = (me, self.steps.get(self.at))
        {
            info!(target: "ai_leader", "tick {tick} step {} {:?} at ({:.1}, {:.1}, {:.1})", self.at + 1, step, at[0], at[1], at[2]);
        }
        if tick % (HZ * 3) == 0 {
            let ups = level_ups(world, snapshot);
            if !ups.is_empty() {
                info!(target: "ai_leader", "tick {tick} taking {} level-up(s)", ups.len());
                actions.extend(ups);
            }
        }
        if std::mem::take(&mut self.party_screen) {
            actions.push(Action::ChooseParty(None));
            info!(target: "ai_leader", "tick {tick} the party table: closed with the party as it stands");
        }
        // Steps that take no time are taken at once.
        while let Some(Step::Say(phrase)) = self.steps.get(self.at).cloned() {
            self.ai.add_say(&phrase);
            self.next(tick, "phrase kept");
        }
        while let Some(Step::Skipped(line)) = self.steps.get(self.at).cloned() {
            info!(target: "ai_leader", "tick {tick} skipping a verb the leader does not do: {line}");
            self.next(tick, "skipped");
        }
        let Some(step) = self.steps.get(self.at).cloned() else {
            if !self.done_logged {
                self.done_logged = true;
                info!(target: "ai_leader", "tick {tick} the plan is done");
            }
            self.ai.set_lead(None);
            return (self.ai.decide(level, snapshot, snapshot, texts, SEAT, tick), actions);
        };
        // A step that has gone nowhere for a while is forced on.
        if let Some(at) = me
            && !matches!(step, Step::Fight(_) | Step::Talk | Step::Say(_))
            && snapshot.talk_for(SEAT).is_none()
            && !self.talked
        {
            let (kept_at, kept_tick) = self.stall;
            if flat(kept_at, at) > STUCK_M || tick < kept_tick {
                self.stall = (at, tick);
            } else if tick >= kept_tick + STUCK_S * HZ && tick >= self.began + STUCK_S * HZ && self.pressed == 0 {
                self.next(tick, "stuck: moving on");
                self.stall = (at, tick);
                return (Command::default(), actions);
            }
        }
        // A step that outlasts its time is given up on.
        if tick >= self.began + STEP_LIMIT_S * HZ && !matches!(step, Step::Fight(_)) {
            self.next(tick, "gave up: out of time");
            return (Command::default(), actions);
        }
        self.ai.hunting = matches!(step, Step::Fight(_));
        let module_changed = level.name() != self.module;
        let in_talk = snapshot.talk_for(SEAT).is_some();
        match &step {
            Step::To(tag) => {
                // A placeable or a door is walked up to where it is used (its
                // middle may lie off the floor); anything else by the seat's
                // own steering to its place.
                if let Some(me) = me
                    && let Some((index, object)) = Self::find(level, tag, 1, me)
                    && matches!(object.kind, ObjectKind::Placeable | ObjectKind::Door)
                {
                    self.ai.set_lead(None);
                    if let Some(door) = self.door_goal {
                        info!(target: "ai_leader", "tick {tick} door goal {door}");
                        let closed = sim::target::entity(snapshot, door).and_then(|entity| entity.object).is_some_and(|state| state.state == 0);
                        if closed {
                            let result = self.use_object(world, snapshot, door, false, false, tick, &mut actions);
                            if tick >= self.began + 2 {
                                info!(target: "ai_leader", "tick {tick} walking to shut door {door}: {}", match &result { Some(Used::Steer(c)) => format!("steer fwd {} right {}", c.forward, c.right), Some(Used::Asked) => "asked".into(), Some(Used::Waiting) => "waiting".into(), Some(Used::Stuck) => "stuck".into(), None => "none".into() });
                            }
                            match result {
                                Some(Used::Steer(command)) => return (command, actions),
                                Some(Used::Asked) | Some(Used::Waiting) => return (Command::default(), actions),
                                Some(Used::Stuck) | None => self.door_goal = None,
                            }
                        } else {
                            self.door_goal = None;
                            self.walk = None;
                        }
                    }
                    match self.use_object(world, snapshot, index, false, true, tick, &mut actions) {
                        Some(Used::Steer(command)) => return (command, actions),
                        Some(Used::Asked) => {
                            self.next(tick, "walked up to it");
                            return (Command::default(), actions);
                        }
                        Some(Used::Waiting) => return (Command::default(), actions),
                        Some(Used::Stuck) | None => {
                            // Every way shut: the nearest closed door close by is
                            // walked up to (to its use point) and opened.
                            let door = snapshot
                                .entities
                                .iter()
                                .filter_map(|entity| match (entity.kind, entity.object) {
                                    (sim::Kind::Object(ObjectKind::Door, number), Some(state)) if number != index && state.state == 0 => {
                                        let near = flat(entity.position, me);
                                        (near < 14.0).then_some((number, near))
                                    }
                                    _ => None,
                                })
                                .min_by(|a, b| a.1.total_cmp(&b.1))
                                .map(|(number, _)| number);
                            info!(target: "ai_leader", "tick {tick} every way shut to {tag}: closed door near {door:?}");
                            for entity in &snapshot.entities {
                                if let (sim::Kind::Object(ObjectKind::Door, number), Some(state)) = (entity.kind, entity.object)
                                    && let Some(object) = level.placed().get(number as usize)
                                {
                                    info!(target: "ai_leader", "door {number}: tag {:?} opens {:?} state {} lock {} at {:?}", object.tag, object.opens, state.state, state.lock, object.position);
                                }
                            }
                            if door.is_some() {
                                self.door_goal = door;
                                self.walk = None;
                                return (Command::default(), actions);
                            }
                        }
                    }
                }
                self.ai.set_lead(Some(tag.clone()));
                let (_, arrived, _) = self.ai.lead_state();
                let close = me.is_some_and(|me| Self::find(level, tag, 1, me).is_some_and(|(_, object)| flat(object.position, me) <= 2.5));
                if arrived || close {
                    self.next(tick, "arrived");
                    return (Command::default(), actions);
                }
            }
            Step::Use(..) | Step::Exit(_) | Step::Unlock(_) => {
                let unlock = matches!(step, Step::Unlock(_));
                let (tag, nth) = match &step {
                    Step::Use(tag, nth) => (tag, nth),
                    Step::Exit(tag) | Step::Unlock(tag) => (tag, &1),
                    _ => unreachable!(),
                };
                let exit = matches!(step, Step::Exit(_));
                if module_changed && exit {
                    self.module = level.name().to_string();
                    self.next(tick, "the area changed");
                    return (Command::default(), actions);
                }
                // A container opens no conversation: one in progress (a terminal's
                // beside it, a call) is not the use's, so it is seen through
                // without ending the step.
                let container = me.is_some_and(|me| Self::find(level, tag, *nth, me).is_some_and(|(_, object)| object.container));
                if in_talk && container && !exit {
                    return (self.ai.decide(level, snapshot, snapshot, texts, SEAT, tick), actions);
                }
                if in_talk && !exit {
                    // The use opened a conversation: it is seen through
                    // (the replies chosen by the policy) before the plan goes on.
                    self.talked = true;
                    self.talk_seen = tick;
                    return (self.ai.decide(level, snapshot, snapshot, texts, SEAT, tick), actions);
                }
                if self.talked && !exit && tick < self.talk_seen + TALK_OVER {
                    // The talk has just ended: waiting out the gap before the
                    // step goes on (pressing again would start it afresh).
                    return (Command::default(), actions);
                }
                if self.talked && tick >= self.talk_seen + TALK_OVER {
                    // A door that answered with a conversation (a lock's
                    // refusal, a companion's warning) is used again until it
                    // is open: the step is the door opening, not the talk.
                    let shut = !exit
                        && me.is_some_and(|me| {
                            Self::find(level, tag, *nth, me).is_some_and(|(index, object)| {
                                object.kind == ObjectKind::Door
                                    && sim::target::entity(snapshot, index)
                                        .and_then(|entity| entity.object)
                                        .is_some_and(|state| state.state == 0 && state.locked(object.opens))
                            })
                        });
                    if shut && self.pressed < 2 * PRESSES {
                        self.talked = false;
                        self.last_press = 0;
                        info!(target: "ai_leader", "tick {tick} step {}: {tag} is still shut after its conversation: using it again", self.at + 1);
                    } else {
                        // Left for an explicit `talk` step to claim when the plan
                        // has one next, as a route's does.
                        self.unclaimed = matches!(self.steps.get(self.at + 1), Some(Step::Talk));
                        self.next(tick, "its conversation ended");
                        return (Command::default(), actions);
                    }
                }
                if let Some(me) = me
                    && let Some((index, object)) = Self::find(level, tag, *nth, me)
                {
                    // A container opened by the first press is emptied and
                    // closed, as a player takes everything and leaves: once
                    // the walk has brought the body within reach, taking
                    // again each second until the party carries more.
                    if object.container && self.pressed >= 1 && tick >= self.last_press + HZ && flat(object.position, me) <= 3.0 {
                        let carried = world
                            .script_state()
                            .items
                            .values()
                            .filter(|item| item.holder == sim::inventory::PARTY)
                            .count();
                        if self.loot == 0 {
                            self.loot = tick.max(1);
                            self.carried_before = carried;
                        }
                        if carried > self.carried_before || tick >= self.loot + 6 * HZ {
                            actions.push(Action::Close(index));
                            let held: Vec<String> = world
                                .script_state()
                                .items
                                .values()
                                .filter(|item| item.holder == sim::inventory::PARTY)
                                .map(|item| format!("{}x{}", item.template, item.stack))
                                .collect();
                            info!(target: "ai_leader", "tick {tick} the party carries {held:?} (was {} items)", self.carried_before);
                            self.next(tick, "container emptied");
                        } else if tick >= self.last_take + HZ {
                            self.last_take = tick;
                            // Opened and emptied in one tick, as the harness's loot step does.
                            actions.push(Action::Use { object: index, verb: sim::action::Verb::Open });
                            actions.push(Action::TakeAll(index));
                            info!(target: "ai_leader", "tick {tick} taking everything from {tag}");
                        }
                        return (Command::default(), actions);
                    }
                    // A door that is open is used: pressing it again would close
                    // it (an open door's first choice is Close).
                    if !exit
                        && !unlock
                        && object.kind == ObjectKind::Door
                        && sim::target::entity(snapshot, index)
                            .and_then(|entity| entity.object)
                            .is_some_and(|state| state.state != 0)
                    {
                        self.unclaimed = matches!(self.steps.get(self.at + 1), Some(Step::Talk)) && self.talked;
                        self.next(tick, "the door is open");
                        return (Command::default(), actions);
                    }
                    // A trigger, or a door open to go out by, is walked into by
                    // the seat's own steering.
                    let open_exit = exit
                        && object.kind == ObjectKind::Door
                        && sim::target::entity(snapshot, index)
                            .and_then(|entity| entity.object)
                            .is_some_and(|state| state.state != 0);
                    if object.kind == ObjectKind::Trigger || open_exit {
                        self.ai.set_lead(Some(tag.clone()));
                    } else if let Some(done) = self.use_object(world, snapshot, index, unlock, false, tick, &mut actions) {
                        match done {
                            Used::Steer(command) => return (command, actions),
                            Used::Asked => {
                                self.pressed += 1;
                                self.last_press = tick;
                                info!(target: "ai_leader", "tick {tick} step {}: asked to use {tag}#{nth} (press {})", self.at + 1, self.pressed);
                                if self.pressed >= 2 * PRESSES && !in_talk {
                                    self.next(tick, "asked, no answer: going on");
                                }
                                return (Command::default(), actions);
                            }
                            Used::Waiting => return (Command::default(), actions),
                            Used::Stuck => {
                                // No walk of its own: the seat's steering goes to it.
                                self.ai.set_lead(Some(tag.clone()));
                            }
                        }
                    } else if unlock {
                        // Nothing to unlock (it is not locked): on.
                        self.next(tick, "not locked");
                        return (Command::default(), actions);
                    } else if self.pressed >= 1 && tick >= self.last_press + 4 * HZ {
                        // Used, and it offers nothing more (a door that
                        // answered with a scene, a lock only a key opens).
                        self.next(tick, "used; it offers nothing more");
                        return (Command::default(), actions);
                    }
                } else if tick >= self.began + 3 * HZ {
                    self.next(tick, "no such object here");
                    return (Command::default(), actions);
                }
            }
            Step::Talk => {
                if self.unclaimed && !in_talk {
                    self.unclaimed = false;
                    self.next(tick, "claimed the conversation just seen");
                    return (Command::default(), actions);
                }
                if in_talk {
                    self.talked = true;
                    self.talk_seen = tick;
                } else if !self.talked && tick >= self.began + 40 * HZ {
                    self.next(tick, "no conversation came");
                    return (Command::default(), actions);
                } else if self.talked && tick >= self.talk_seen + TALK_OVER {
                    self.next(tick, "the conversation ended");
                    return (Command::default(), actions);
                }
            }
            Step::Fight(seconds) => {
                // The fight is over when its time is up and no enemy has
                // been fought for a few seconds (at most three times as long).
                let quiet = tick >= self.ai.engaged_at() + 3 * HZ;
                if (tick >= self.began + seconds * HZ && quiet) || tick >= self.began + 3 * seconds * HZ {
                    self.next(tick, "the fight is over");
                    return (Command::default(), actions);
                }
            }
            Step::At(x, y) => {
                let z = me.map_or(0.0, |me| me[2]);
                self.ai.set_lead_point([*x, *y, z]);
                // Arrived by the leader's own measure: the seat's flag is not
                // set while it fights something it cannot reach.
                let close = me.is_some_and(|me| flat(me, [*x, *y, z]) <= 2.5);
                if self.ai.lead_state().1 || close {
                    self.next(tick, "arrived");
                    return (Command::default(), actions);
                }
            }
            Step::Leader(who) => {
                let npc = who.unwrap_or(u8::MAX);
                let led = world.script_state().party.leader == Some(npc).filter(|npc| *npc != u8::MAX);
                if led || (npc == u8::MAX && world.script_state().party.leader.is_none()) {
                    self.next(tick, "leader set");
                    return (Command::default(), actions);
                }
                // Asked again while not yet led, as a player clicks the
                // portrait again.
                if tick >= self.last_press + PRESS_EVERY {
                    self.last_press = tick;
                    actions.push(lead_to(world, snapshot, npc));
                }
                return (Command::default(), actions);
            }
            Step::Equip(named) => {
                // Asked again until the party carries something to wear (a
                // container's take lands a tick or two late).
                let wear = equip(world, snapshot, named.as_deref());
                if tick % (HZ * 2) == 0 {
                    let held: Vec<String> = world
                        .script_state()
                        .items
                        .values()
                        .filter(|item| item.holder == sim::inventory::PARTY)
                        .map(|item| item.template.clone())
                        .collect();
                    info!(target: "ai_leader", "tick {tick} equip: the party carries {held:?}, me {:?}", crate::cast::me(snapshot, SEAT));
                }
                if !wear.is_empty() {
                    actions.extend(wear);
                    self.next(tick, "wearing what the party carries");
                } else if tick >= self.began + 10 * HZ {
                    self.next(tick, "nothing to wear");
                }
                return (Command::default(), actions);
            }
            Step::Say(_) | Step::Skipped(_) => {}
        }
        let command = self.ai.decide(level, snapshot, snapshot, texts, SEAT, tick);
        if tick % (HZ * 2) == 0 {
            info!(target: "ai_leader", "tick {tick} command: forward {} right {} angles {:?} buttons {:?} reply {}", command.forward, command.right, command.angles, command.buttons, command.reply);
        }
        (command, actions)
    }
}

/// The level-ups the party's XP allows, as the game's Auto Level Up takes
/// them: the leader's own (in the class it took last, else the first) and
/// each companion no seat plays. Asked again each time the leader looks, so
/// a second level follows the first.
fn level_ups(world: &sim::SimWorld, snapshot: &Snapshot) -> Vec<Action> {
    let mut acts = Vec::new();
    let Some(library) = world.level().scripts() else {
        return acts;
    };
    let state = world.script_state();
    let xp = state.xp(sim::inventory::PARTY);
    if let Some(me) = crate::cast::me(snapshot, SEAT)
        && let Some(sheet) = snapshot.get(me).and_then(|entity| entity.sheet)
        && library.classes().may_level(sheet.level, xp)
    {
        let class = state
            .careers
            .get(&me.0)
            .and_then(|career| career.last().map(|(class, _)| *class))
            .unwrap_or(0);
        acts.push(Action::LevelUp(sim::levels::Choices { class, ..sim::levels::Choices::default() }));
    }
    let instances = sim::Instances::in_snapshot(world.level(), snapshot);
    for npc in state.party.available.values() {
        let Some(id) = npc.object else { continue };
        let Some(entity) = snapshot.get(sim::SimId(id)) else { continue };
        let Some(data) = instances.of_entity(entity) else { continue };
        let Some(sheet) = entity.sheet.or(data.sheet) else { continue };
        let Some(followed) = entity.fighter.map(|fighter| fighter.mind) else { continue };
        if followed.leader != Some(SEAT) || followed.driver.is_some() || !library.classes().may_level(sheet.level, xp) {
            continue;
        }
        let career = match state.careers.get(&id) {
            Some(career) => career.clone(),
            None if data.classes.is_empty() => continue,
            None => sim::levels::untaken(&data.classes, sheet.level, 0),
        };
        let Some(&(class, _)) = career.last() else { continue };
        let banked = state.unspent.get(&id).copied().unwrap_or(0);
        if let Some(choices) = library.classes().auto(&sheet, &career, class, banked) {
            acts.push(Action::LevelUpOn { who: id, choices });
        }
    }
    acts
}

/// Wear what the party carries (one item, by template, when named): what
/// the equip screen would do for the seat's own character.
fn equip(world: &sim::SimWorld, snapshot: &Snapshot, named: Option<&str>) -> Vec<Action> {
    let Some(me) = crate::cast::me(snapshot, SEAT) else {
        return Vec::new();
    };
    let Some(library) = world.level().scripts() else {
        return Vec::new();
    };
    let race = snapshot.get(me).and_then(|e| e.sheet).map(|sheet| sheet.race);
    let state = world.script_state();
    let mut worn: u32 = state
        .items
        .values()
        .filter(|item| item.holder == me.0)
        .filter_map(|item| item.slot)
        .fold(0, |worn, slot| worn | 1 << slot);
    let mut wear = Vec::new();
    for (id, item) in state
        .items
        .iter()
        .filter(|(_, item)| item.holder == sim::inventory::PARTY)
        .filter(|(_, item)| named.is_none_or(|named| item.is(named)))
    {
        let slots = state
            .item_facts(library.items(), &item.template)
            .filter(|facts| race.is_none_or(|race| facts.fits(race)))
            .map_or(0, |facts| facts.slots);
        if named.is_some() {
            if let Some(slot) = (0..32u8).find(|bit| slots & (1 << bit) != 0) {
                return vec![Action::Equip { item: *id, slot }];
            }
            continue;
        }
        if let Some(slot) = (0..32u8).find(|bit| slots & !worn & (1 << bit) != 0) {
            worn |= 1 << slot;
            wear.push(Action::Equip { item: *id, slot });
        }
    }
    wear
}

/// The action that makes party member `npc` (`u8::MAX`: the player) lead.
fn lead_to(world: &sim::SimWorld, snapshot: &Snapshot, npc: u8) -> Action {
    let body = match npc {
        u8::MAX => snapshot.own(SEAT).map(|own| own.id),
        npc => world
            .script_state()
            .party
            .available
            .get(&npc)
            .and_then(|known| known.object)
            .map(sim::SimId),
    };
    body.map_or(Action::SetLeader(npc), Action::Lead)
}

fn flat(a: [f32; 3], b: [f32; 3]) -> f32 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2)).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_route_verbs() {
        let plan = parse(
            "# comment\nparty 8,2\nto kas25_kinrath\nuse kas25_kinrath@2 # x\nexit KashyyykDoor3\ntalk\nsay going past you\nfight 15\nfight\n",
        );
        assert_eq!(plan[0], Step::Skipped("party 8,2".into()));
        assert_eq!(plan[1], Step::To("kas25_kinrath".into()));
        assert_eq!(plan[2], Step::Use("kas25_kinrath".into(), 2));
        assert_eq!(plan[3], Step::Exit("KashyyykDoor3".into()));
        assert_eq!(plan[4], Step::Talk);
        assert_eq!(plan[5], Step::Say("going past you".into()));
        assert_eq!(plan[6], Step::Fight(15));
        assert_eq!(plan[7], Step::Fight(FIGHT_S));
    }
}
