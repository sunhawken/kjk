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
const STEP_LIMIT_S: u64 = 60;
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
    fn use_object(&mut self, world: &sim::SimWorld, snapshot: &Snapshot, index: u32, tick: u64, actions: &mut Vec<Action>) -> Option<Used> {
        use sim::target::{Spot, Stride, Walk};
        let level = world.level();
        let me = snapshot.body(SEAT)?;
        let ps = me.player?;
        let entity = sim::target::entity(snapshot, index)?;
        let instances = sim::Instances::in_snapshot(level, snapshot);
        let facts = instances.of_entity(entity)?;
        let state = entity.object.unwrap_or_default();
        let choices = sim::target::choices(facts.kind, facts, state, false, None, None);
        let choice = *choices.first()?;
        let feet = sim::target::candidates(level, snapshot, Some(me.id))
            .into_iter()
            .find(|candidate| candidate.index == index)?
            .feet;
        let spot = Spot::in_snapshot(level, snapshot, (index, facts.kind), choice, feet, &ps);
        let doors = |number: u32| {
            sim::target::entity(snapshot, number)
                .and_then(|entity| entity.object)
                .unwrap_or_default()
        };
        let none: Vec<bool> = Vec::new();
        let ways = sim::walk::Walled { doors: &doors, walled: &none };
        let now_ms = snapshot.time_ms;
        if tick < self.last_press + PRESS_EVERY && self.pressed > 0 {
            return Some(Used::Waiting);
        }
        if self.walk.is_none() {
            if Walk::near(&ps, spot) {
                actions.extend(sim::target::ask(choice, index, facts.kind));
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
                actions.extend(sim::target::ask(choice, index, facts.kind));
                Some(Used::Asked)
            }
            Stride::Unreachable => {
                self.walk = None;
                info!(target: "ai_leader", "tick {tick} the walk to object {index} cannot reach it");
                Some(Used::Asked)
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
        if std::mem::take(&mut self.party_screen) {
            actions.push(Action::ChooseParty(None));
            info!(target: "ai_leader", "tick {tick} the party table: closed with the party as it stands");
        }
        // Steps that take no time are taken at once.
        while let Some(Step::Say(phrase)) = self.steps.get(self.at).cloned() {
            self.ai.add_say(&phrase);
            self.next(tick, "phrase kept");
        }
        while let Some(Step::Unlock(tag)) = self.steps.get(self.at).cloned() {
            if let Some(me) = me
                && let Some((object, _)) = Self::find(level, &tag, 1, me)
            {
                actions.push(Action::Unlock { object, spike: None });
            }
            self.next(tick, "unlock tried");
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
        // A step that outlasts its time is given up on.
        if tick >= self.began + STEP_LIMIT_S * HZ && !matches!(step, Step::Fight(_)) {
            self.next(tick, "gave up: out of time");
            return (Command::default(), actions);
        }
        let module_changed = level.name() != self.module;
        let in_talk = snapshot.talk_for(SEAT).is_some();
        match &step {
            Step::To(tag) => {
                self.ai.set_lead(Some(tag.clone()));
                let (_, arrived, _) = self.ai.lead_state();
                if arrived {
                    self.next(tick, "arrived");
                    return (Command::default(), actions);
                }
            }
            Step::Use(..) | Step::Exit(_) => {
                let (tag, nth) = match &step {
                    Step::Use(tag, nth) => (tag, nth),
                    Step::Exit(tag) => (tag, &1),
                    _ => unreachable!(),
                };
                let exit = matches!(step, Step::Exit(_));
                if module_changed && exit {
                    self.module = level.name().to_string();
                    self.next(tick, "the area changed");
                    return (Command::default(), actions);
                }
                if in_talk && !exit {
                    // The use opened a conversation: it is seen through
                    // (the replies chosen by the policy) before the plan goes on.
                    self.talked = true;
                    self.talk_seen = tick;
                    return (self.ai.decide(level, snapshot, snapshot, texts, SEAT, tick), actions);
                }
                if self.talked && tick >= self.talk_seen + TALK_OVER {
                    // Left for an explicit `talk` step to claim when the plan
                    // has one next, as a route's does.
                    self.unclaimed = matches!(self.steps.get(self.at + 1), Some(Step::Talk));
                    self.next(tick, "its conversation ended");
                    return (Command::default(), actions);
                }
                if let Some(me) = me
                    && let Some((index, object)) = Self::find(level, tag, *nth, me)
                {
                    // A container opened by the first press is emptied and
                    // closed, as a player takes everything and leaves: once
                    // the walk has brought the body within reach, taking
                    // again each second until the party carries more.
                    if object.container && self.pressed >= 1 && flat(object.position, me) <= 2.5 {
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
                    // A trigger is walked into by the seat's own steering.
                    if object.kind == ObjectKind::Trigger {
                        self.ai.set_lead(Some(tag.clone()));
                    } else if let Some(done) = self.use_object(world, snapshot, index, tick, &mut actions) {
                        match done {
                            Used::Steer(command) => return (command, actions),
                            Used::Asked => {
                                self.pressed += 1;
                                self.last_press = tick;
                                info!(target: "ai_leader", "tick {tick} step {}: asked to use {tag}#{nth} (press {})", self.at + 1, self.pressed);
                                if self.pressed >= PRESSES && !in_talk {
                                    self.next(tick, "asked, no answer: going on");
                                }
                                return (Command::default(), actions);
                            }
                            Used::Waiting => return (Command::default(), actions),
                        }
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
                if tick >= self.began + seconds * HZ {
                    self.next(tick, "the time is up");
                    return (Command::default(), actions);
                }
            }
            Step::At(x, y) => {
                let z = me.map_or(0.0, |me| me[2]);
                self.ai.set_lead_point([*x, *y, z]);
                if self.ai.lead_state().1 {
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
            Step::Say(_) | Step::Skipped(_) | Step::Unlock(_) => {}
        }
        let command = self.ai.decide(level, snapshot, snapshot, texts, SEAT, tick);
        if tick % (HZ * 2) == 0 {
            info!(target: "ai_leader", "tick {tick} command: forward {} right {} angles {:?} buttons {:?} reply {}", command.forward, command.right, command.angles, command.buttons, command.reply);
        }
        (command, actions)
    }
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
