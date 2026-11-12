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

use sim::action::{Action, Verb};
use sim::module::ObjectKind;
use sim::{Command, Level, Seat, Snapshot};
use bevy::prelude::{error, info};

use crate::ai_seat::{AiSeat, Delegate};

/// The leader's seat: the host's.
const SEAT: Seat = Seat(0);
/// Play time before a step is given up on.
const STEP_LIMIT_S: u64 = 90;
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
/// Within this many metres a use is pressed (the sim checks its own reach).
const USE_WITHIN_M: f32 = 3.5;

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
    /// The end of the plan was logged.
    done_logged: bool,
    /// A party table was shown and is not yet closed.
    party_screen: bool,
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
            done_logged: false,
            party_screen: false,
        }
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

    /// The seat's command this tick, and the actions it takes. `texts` are
    /// the reply texts of the conversation the seat is in.
    pub fn command(&mut self, level: &Level, snapshot: &Snapshot, texts: &[String], tick: u64) -> (Command, Vec<Action>) {
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
        while let Some(Step::Leader(who)) = self.steps.get(self.at).cloned() {
            actions.push(Action::SetLeader(who.unwrap_or(u8::MAX)));
            self.next(tick, "leader set");
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
                    // The use opened a conversation: the next `talk` step
                    // (or the policy alone) sees it through.
                    self.next(tick, "a conversation began");
                    return (self.ai.decide(level, snapshot, snapshot, texts, SEAT, tick), actions);
                }
                if let Some(me) = me
                    && let Some((index, object)) = Self::find(level, tag, *nth, me)
                {
                    let near = flat(object.position, me) <= USE_WITHIN_M;
                    // A trigger is walked into, not used.
                    let walk_in = object.kind == ObjectKind::Trigger;
                    if !near || walk_in {
                        // The lead walks to the nearest with the tag.
                        self.ai.set_lead(Some(tag.clone()));
                    } else {
                        self.ai.set_lead(None);
                        if tick >= self.last_press + PRESS_EVERY {
                            let verb = match object.kind {
                                ObjectKind::Door => Verb::Open,
                                ObjectKind::Creature => Verb::Talk,
                                _ => Verb::Use,
                            };
                            actions.push(Action::Use { object: index, verb });
                            self.pressed += 1;
                            self.last_press = tick;
                            info!(target: "ai_leader", "tick {tick} step {}: {verb:?} {tag}#{nth} (press {})", self.at + 1, self.pressed);
                        }
                        if self.pressed >= PRESSES && !in_talk {
                            self.next(tick, "pressed, no answer: going on");
                            return (Command::default(), actions);
                        }
                    }
                } else if tick >= self.began + 3 * HZ {
                    self.next(tick, "no such object here");
                    return (Command::default(), actions);
                }
            }
            Step::Talk => {
                if in_talk {
                    self.talked = true;
                    self.talk_seen = tick;
                } else if self.talked && tick >= self.talk_seen + TALK_OVER {
                    self.next(tick, "the conversation ended");
                    return (Command::default(), actions);
                } else if !self.talked && tick >= self.began + 4 * HZ {
                    self.next(tick, "no conversation came");
                    return (Command::default(), actions);
                }
            }
            Step::Fight(seconds) => {
                if tick >= self.began + seconds * HZ {
                    self.next(tick, "the time is up");
                    return (Command::default(), actions);
                }
            }
            Step::Say(_) | Step::Skipped(_) | Step::Leader(_) | Step::Unlock(_) => {}
        }
        (self.ai.decide(level, snapshot, snapshot, texts, SEAT, tick), actions)
    }
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
