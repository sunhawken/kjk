//! Quest markers: where the main quest goes next, shown in the world and on
//! the map page. The way is the K1 walkthrough routes (`data/routes/k1/*.route`,
//! embedded here): each step that names a door, a placeable, a creature, a
//! trigger or a point of the current area is an objective, taken in story
//! order; the first one not yet done is the current objective, a few more
//! to come follow it on the map.
//!
//! The area decides which steps apply: a stretch of a route (between its
//! `exit`s and `module`s) belongs to an area when most of the objects it
//! names stand there. Progress is read from the world: a door opened, a
//! point or object reached, a fight over. A step later than the current one
//! that is plainly done (a door open, a point reached) moves it on, so a
//! loaded save or a different order does not leave a marker behind the
//! player. Off with `KJK_QUEST_MARKERS=0`.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use sim::module::ObjectKind;
use sim::{Kind, Seat, Snapshot};

/// The routes in story order, main way first, then the branches.
const ROUTES: &[&str] = &[
    include_str!("../../../data/routes/k1/endar_spire.route"),
    include_str!("../../../data/routes/k1/taris_upper_city.route"),
    include_str!("../../../data/routes/k1/taris_lower_city.route"),
    include_str!("../../../data/routes/k1/taris_undercity.route"),
    include_str!("../../../data/routes/k1/taris_sewers.route"),
    include_str!("../../../data/routes/k1/taris_vulkar_base.route"),
    include_str!("../../../data/routes/k1/taris_swoop.route"),
    include_str!("../../../data/routes/k1/taris_sith_base.route"),
    include_str!("../../../data/routes/k1/taris_escape.route"),
    include_str!("../../../data/routes/k1/taris_pazaak.route"),
    include_str!("../../../data/routes/k1/dantooine.route"),
    include_str!("../../../data/routes/k1/tatooine.route"),
    include_str!("../../../data/routes/k1/kashyyyk.route"),
    include_str!("../../../data/routes/k1/manaan.route"),
    include_str!("../../../data/routes/k1/leviathan.route"),
    include_str!("../../../data/routes/k1/korriban.route"),
    include_str!("../../../data/routes/k1/unknown_world.route"),
    include_str!("../../../data/routes/k1/unknown_world_temple.route"),
    include_str!("../../../data/routes/k1/star_forge.route"),
    include_str!("../../../data/routes/k1/dantooine_juhani_slain.route"),
    include_str!("../../../data/routes/k1/tatooine_sand_war.route"),
    include_str!("../../../data/routes/k1/kashyyyk_chuundar.route"),
    include_str!("../../../data/routes/k1/manaan_poison.route"),
    include_str!("../../../data/routes/k1/korriban_both.route"),
    include_str!("../../../data/routes/k1/korriban_uthar_side.route"),
    include_str!("../../../data/routes/k1/korriban_yuthura_slain.route"),
    include_str!("../../../data/routes/k1/unknown_world_elders.route"),
    include_str!("../../../data/routes/k1/unknown_world_dark.route"),
    include_str!("../../../data/routes/k1/star_forge_dark.route"),
];

/// How near (metres, across the floor) counts as arrived.
const ARRIVED_M: f32 = 2.5;
/// How near a later step's point must be to move the current one on to it.
const PASSED_M: f32 = 1.5;
/// How many steps on a step plainly done moves the objective from.
const LOOK_AHEAD: usize = 6;
/// How many objectives the map page shows, the current one first.
const ON_MAP: usize = 4;

#[derive(Debug, Clone, PartialEq)]
enum Goal {
    /// Use or open a door, a placeable or a creature by tag (the nth nearest).
    Use { tag: String, nth: usize },
    /// Pick the lock of a door by tag.
    Unlock { tag: String },
    /// Walk up to an object by tag.
    To { tag: String },
    /// Leave by a door or trigger by tag.
    Exit { tag: String },
    /// Go to a point of the floor.
    At { x: f32, y: f32 },
    /// Win the fight that is on.
    Fight,
}

#[derive(Debug, Clone, PartialEq)]
struct Step {
    goal: Goal,
    /// The route's own words for it (its comment).
    words: String,
    /// Which stretch of its route it is in.
    stretch: usize,
}

/// What the player is to do next.
#[derive(Debug, Clone, PartialEq)]
pub struct Objective {
    /// Game space, metres.
    pub at: [f32; 3],
    /// What to do, short.
    pub label: String,
}

/// Every step of every route, tagged with its stretch (stretches number on
/// across the routes).
fn steps() -> &'static [Step] {
    static STEPS: OnceLock<Vec<Step>> = OnceLock::new();
    STEPS.get_or_init(|| {
        let mut steps = Vec::new();
        let mut stretch = 0;
        for text in ROUTES {
            stretch += 1;
            for line in text.lines() {
                let (code, words) = line.split_once('#').unwrap_or((line, ""));
                let code = code.trim();
                if code.is_empty() {
                    continue;
                }
                let words = words.trim().to_string();
                let (verb, rest) = code.split_once(char::is_whitespace).unwrap_or((code, ""));
                let rest = rest.trim();
                let (tag, nth) = match rest.split_once('@') {
                    Some((tag, n)) => (tag.trim().to_string(), n.trim().parse().unwrap_or(1usize)),
                    None => (rest.to_string(), 1),
                };
                let goal = match verb {
                    "use" if !rest.is_empty() => Goal::Use { tag, nth },
                    "unlock" if !rest.is_empty() => Goal::Unlock { tag },
                    "to" if !rest.is_empty() => Goal::To { tag },
                    "exit" if !rest.is_empty() => Goal::Exit { tag },
                    "fight" => Goal::Fight,
                    "at" => {
                        let mut n = rest.split_whitespace().filter_map(|v| v.parse::<f32>().ok());
                        match (n.next(), n.next()) {
                            (Some(x), Some(y)) => Goal::At { x, y },
                            _ => continue,
                        }
                    }
                    // Leaving the area ends the stretch; the next is another's.
                    "module" | "travel" => {
                        stretch += 1;
                        continue;
                    }
                    _ => continue,
                };
                let leaves = matches!(goal, Goal::Exit { .. });
                steps.push(Step { goal, words, stretch });
                if leaves {
                    stretch += 1;
                }
            }
        }
        steps
    })
}

fn flat(a: [f32; 3], b: [f32; 3]) -> f32 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2)).sqrt()
}

/// The `nth` nearest placed object with `tag` (its number and place).
fn find(level: &sim::Level, tag: &str, nth: usize, from: [f32; 3]) -> Option<(u32, [f32; 3], ObjectKind)> {
    let mut found: Vec<_> = level
        .placed()
        .iter()
        .enumerate()
        .filter(|(_, object)| object.tag.eq_ignore_ascii_case(tag))
        .map(|(number, object)| (number as u32, object.position, object.kind))
        .collect();
    found.sort_by(|a, b| flat(a.1, from).total_cmp(&flat(b.1, from)));
    found.into_iter().nth(nth.saturating_sub(1))
}

fn tag_of(goal: &Goal) -> Option<&str> {
    match goal {
        Goal::Use { tag, .. } | Goal::Unlock { tag } | Goal::To { tag } | Goal::Exit { tag } => Some(tag),
        Goal::At { .. } | Goal::Fight => None,
    }
}

/// The steps that apply in `level`: those in the stretches most of whose
/// named objects stand in it, in story order.
fn applicable(level: &sim::Level) -> Vec<Step> {
    let mut named: HashMap<usize, (usize, usize)> = HashMap::new();
    for step in steps() {
        if let Some(tag) = tag_of(&step.goal) {
            let entry = named.entry(step.stretch).or_default();
            entry.1 += 1;
            if level.placed().iter().any(|object| object.tag.eq_ignore_ascii_case(tag)) {
                entry.0 += 1;
            }
        }
    }
    steps()
        .iter()
        .filter(|step| named.get(&step.stretch).is_some_and(|(found, all)| *found > 0 && *found * 2 >= *all))
        .filter(|step| tag_of(&step.goal).is_none_or(|tag| level.placed().iter().any(|object| object.tag.eq_ignore_ascii_case(tag))))
        .cloned()
        .collect()
}

/// Something that may be needed lying in the area: items carried by a
/// container or a creature.
#[derive(Debug, Clone, PartialEq)]
pub struct Pickup {
    /// Game space, metres.
    pub at: [f32; 3],
    /// What it is and where: "Computer Spikes (footlocker)".
    pub label: String,
}

/// An object of the area holding items that may be needed, as the area's
/// data has it.
struct Stash {
    number: u32,
    kind: ObjectKind,
    label: String,
}

/// Words in an item's tag or template that mark one a quest may ask for.
const NEEDED_WORDS: &[&str] = &[
    "key", "spike", "card", "datapad", "sample", "disk", "tome", "holocron", "medal", "badge", "ticket", "fuse",
];

/// How many pickups to keep, nearest first.
const PICKUPS: usize = 6;

#[derive(Default)]
struct State {
    /// The area's holders of items that may be needed, and those the player
    /// has been up to.
    stashes: Vec<Stash>,
    taken: std::collections::HashSet<u32>,
    pickups: Vec<Pickup>,
    level: String,
    steps: Vec<Step>,
    /// The current step, by index into `steps`.
    at: usize,
    /// Where each area's progress stood when it was left.
    left: HashMap<String, usize>,
    /// The objectives to come from the current one, resolved.
    ahead: Vec<Objective>,
}

/// The area's holders of items that may be needed: an item the game marks
/// plot or whose tag names a key, spike, card or the like, or any container
/// a walkthrough step uses.
fn scan(level: &sim::Level, steps: &[Step]) -> Vec<Stash> {
    let library = level.scripts();
    let routed: Vec<&str> = steps
        .iter()
        .filter_map(|step| match &step.goal {
            Goal::Use { tag, .. } => Some(tag.as_str()),
            _ => None,
        })
        .collect();
    level
        .placed()
        .iter()
        .enumerate()
        .filter(|(_, object)| matches!(object.kind, ObjectKind::Placeable | ObjectKind::Creature) && !object.inventory.is_empty())
        .filter_map(|(number, object)| {
            let names: Vec<String> = object
                .inventory
                .iter()
                .filter_map(|held| {
                    let facts = library.and_then(|library| library.items().get(&held.template));
                    let tag = facts.map_or(held.template.to_ascii_lowercase(), |facts| facts.tag.to_ascii_lowercase());
                    let words = NEEDED_WORDS.iter().any(|word| tag.contains(word) || held.template.contains(word));
                    let plot = facts.is_some_and(|facts| facts.plot);
                    (plot || words).then(|| {
                        facts
                            .and_then(|facts| library.and_then(|library| library.text(facts.name)))
                            .filter(|text| !text.trim().is_empty())
                            .unwrap_or_else(|| held.template.clone())
                    })
                })
                .collect();
            let wanted = routed.iter().any(|tag| object.tag.eq_ignore_ascii_case(tag));
            if names.is_empty() && !(wanted && object.kind == ObjectKind::Placeable) {
                return None;
            }
            let holder = if object.name.is_empty() { object.tag.clone() } else { object.name.clone() };
            let what = match names.as_slice() {
                [] => "Items".to_string(),
                [one] => one.clone(),
                [first, rest @ ..] => format!("{first} +{}", rest.len()),
            };
            Some(Stash { number: number as u32, kind: object.kind, label: format!("{what} ({holder})") })
        })
        .collect()
}

fn state() -> &'static Mutex<State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(State::default()))
}

/// Where `step` is in `level` now, and whether it is done.
fn read(level: &sim::Level, snapshot: &Snapshot, me: [f32; 3], step: &Step) -> Option<(Objective, bool, bool)> {
    let label = |fallback: String| if step.words.is_empty() { fallback } else { short(&step.words) };
    match &step.goal {
        Goal::At { x, y } => {
            let at = [*x, *y, me[2]];
            let near = flat(at, me);
            Some((Objective { at, label: label("Go on".into()) }, near <= ARRIVED_M, near <= PASSED_M))
        }
        Goal::Fight => {
            let foe = snapshot
                .entities
                .iter()
                .filter(|entity| {
                    matches!(entity.kind, Kind::Object(ObjectKind::Creature, _))
                        && !snapshot.in_party(entity)
                        && entity.fighter.as_ref().is_some_and(|fighter| fighter.alive() && fighter.combat_until > snapshot.time_ms)
                })
                .min_by(|a, b| flat(a.position, me).total_cmp(&flat(b.position, me)));
            match foe {
                Some(foe) => Some((Objective { at: foe.position, label: label("Fight".into()) }, false, false)),
                None => Some((Objective { at: me, label: label("Fight".into()) }, true, false)),
            }
        }
        goal => {
            let (tag, nth) = match goal {
                Goal::Use { tag, nth } => (tag, *nth),
                Goal::Unlock { tag } | Goal::To { tag } | Goal::Exit { tag } => (tag, 1),
                _ => return None,
            };
            let (number, at, kind) = find(level, tag, nth, me)?;
            let entity = sim::target::entity(snapshot, number);
            let open = entity.and_then(|entity| entity.object).is_some_and(|state| state.state != 0);
            let gone = entity.is_none_or(|entity| entity.fighter.as_ref().is_some_and(|fighter| !fighter.alive()));
            let near = flat(at, me);
            let verb = match goal {
                Goal::Exit { .. } => "Leave by",
                Goal::To { .. } => "Go to",
                Goal::Unlock { .. } => "Unlock",
                _ if kind == ObjectKind::Creature => "Talk to",
                _ => "Use",
            };
            let fallback = format!("{verb} {tag}");
            let objective = Objective { at, label: label(fallback) };
            let (done, passed) = match (goal, kind) {
                // An exit is done by leaving: the area changes.
                (Goal::Exit { .. }, _) => (false, false),
                (_, ObjectKind::Door) => (open, open),
                (_, ObjectKind::Creature) => (gone || near <= ARRIVED_M + 0.5, gone),
                _ => (near <= ARRIVED_M - 0.5, near <= PASSED_M),
            };
            Some((objective, done, passed))
        }
    }
}

/// The objective's words: its route comment, short.
fn short(words: &str) -> String {
    let words = words.split(['(', ';', ':']).next().unwrap_or(words).trim();
    let mut out: String = words.chars().take(48).collect();
    if out.is_empty() {
        out = "Next objective".into();
    }
    out
}

/// Follows the player through the area's objectives: the current one and the
/// few after it. Called each frame from the world's view.
pub fn update(level: &sim::Level, snapshot: &Snapshot, seat: Seat) {
    if std::env::var("KJK_QUEST_MARKERS").is_ok_and(|value| value == "0") {
        return;
    }
    let Some(me) = snapshot.body(seat).map(|body| body.position) else {
        return;
    };
    let mut state = state().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if state.level != level.name() {
        let name = std::mem::take(&mut state.level);
        let at = state.at;
        if !name.is_empty() {
            state.left.insert(name, at);
        }
        state.level = level.name().to_string();
        state.steps = applicable(level);
        state.at = state.left.get(&state.level).copied().unwrap_or(0).min(state.steps.len());
        state.stashes = scan(level, &state.steps);
        state.taken.clear();
    }
    // On past what is done, and past a step plainly done a little further.
    let mut guard = 0;
    while let Some(step) = state.steps.get(state.at) {
        guard += 1;
        if guard > 200 {
            break;
        }
        match read(level, snapshot, me, step) {
            Some((_, true, _)) | None => state.at += 1,
            Some(_) => break,
        }
    }
    let from = state.at;
    let passed = (from + 1..(from + 1 + LOOK_AHEAD).min(state.steps.len()))
        .rev()
        .find(|&index| read(level, snapshot, me, &state.steps[index]).is_some_and(|(_, _, passed)| passed));
    if let Some(index) = passed {
        state.at = index + 1;
    }
    let from = state.at;
    let ahead: Vec<Objective> = state.steps[from.min(state.steps.len())..]
        .iter()
        .filter_map(|step| read(level, snapshot, me, step))
        .filter(|(_, done, _)| !done)
        .map(|(objective, ..)| objective)
        .fold(Vec::new(), |mut all: Vec<Objective>, objective| {
            if all.len() < ON_MAP && all.last().is_none_or(|last| flat(last.at, objective.at) > 1.0) {
                all.push(objective);
            }
            all
        });
    if state.ahead.first() != ahead.first() {
        if let Some(first) = ahead.first() {
            bevy::log::info!(target: "quest", "objective {} at ({:.1}, {:.1}) in {} ({} more)", first.label, first.at[0], first.at[1], level.name(), ahead.len() - 1);
        }
    }
    state.ahead = ahead;
    // What may be needed and not yet gathered: gone from the list once the
    // player has been up to it (a container) or to its remains (a creature).
    let mut found = Vec::new();
    let mut taken = Vec::new();
    for stash in &state.stashes {
        if state.taken.contains(&stash.number) {
            continue;
        }
        let entity = sim::target::entity(snapshot, stash.number);
        let Some(at) = entity.map(|entity| entity.position).or_else(|| level.placed().get(stash.number as usize).map(|object| object.position)) else {
            continue;
        };
        let dead = entity.is_some_and(|entity| entity.fighter.as_ref().is_some_and(|fighter| !fighter.alive()));
        let near = flat(at, me);
        let reached = near <= if stash.kind == ObjectKind::Creature { ARRIVED_M } else { 2.0 };
        if reached && (stash.kind != ObjectKind::Creature || dead) {
            taken.push(stash.number);
            continue;
        }
        found.push((near, Pickup { at, label: stash.label.clone() }));
    }
    state.taken.extend(taken);
    found.sort_by(|a, b| a.0.total_cmp(&b.0));
    state.pickups = found.into_iter().take(PICKUPS).map(|(_, pickup)| pickup).collect();
}

/// The objectives to come in the area `level` names (the current one first),
/// as last followed; none for another area.
pub fn upcoming(level: &str) -> Vec<Objective> {
    let state = state().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if state.level == level { state.ahead.clone() } else { Vec::new() }
}

/// The items that may be needed lying in the area `level` names, nearest
/// first, as last followed.
pub fn pickups(level: &str) -> Vec<Pickup> {
    let state = state().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if state.level == level { state.pickups.clone() } else { Vec::new() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_routes_parse_into_stretches_with_words() {
        let all = steps();
        assert!(all.len() > 500, "{} steps", all.len());
        let door = all.iter().find(|step| matches!(&step.goal, Goal::Use { tag, .. } if tag == "end_door04")).unwrap();
        assert!(door.words.contains("pods"), "{door:?}");
        assert!(all.iter().any(|step| matches!(step.goal, Goal::At { .. })));
    }

    #[test]
    fn short_keeps_the_first_clause() {
        assert_eq!(short("the pods' door (Carth behind it)"), "the pods' door");
        assert_eq!(short(""), "Next objective");
    }
}
