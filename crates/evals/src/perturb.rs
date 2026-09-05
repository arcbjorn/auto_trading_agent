//! Deterministic perturbations of a case's turns, for prompt robustness beyond the hand-written
//! paraphrases: casing, noise around the request, and typos in ordinary words.
//!
//! Numbers are never touched, nor are the words the service's own gate looks for (trade and cancel
//! verbs, sides, the asset, confirmations). What is measured is the model's reading of everything
//! else, not the gate's vocabulary.
//!
//! The same case, turn and rep always get the same text.

pub const KINDS: [&str; 4] = ["casing", "noise", "typos", "all"];

const PREFIXES: [&str; 6] = ["hey, ", "hi! ", "ok so ", "quick one: ", "pls ", "yo "];
const SUFFIXES: [&str; 6] = [" thanks", " pls", " please", "!!", " thx", " ..."];

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

/// One seed per case, turn and rep, so reruns reproduce a run and reps differ from each other.
pub fn seed(case_id: &str, turn: usize, rep: u32) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in format!("{case_id}/{turn}/{rep}").bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h | 1
}

pub fn apply(kind: &str, text: &str, seed: u64) -> anyhow::Result<String> {
    let mut rng = Rng(seed);
    Ok(match kind {
        "casing" => casing(text, &mut rng),
        "noise" => noise(text, &mut rng),
        "typos" => typos(text, &mut rng),
        "all" => {
            let t = typos(text, &mut rng);
            let t = noise(&t, &mut rng);
            casing(&t, &mut rng)
        }
        other => anyhow::bail!("unknown perturbation {other}; use one of {}", KINDS.join(", ")),
    })
}

/// All upper case, all lower case, or every other word capitalised.
fn casing(text: &str, rng: &mut Rng) -> String {
    match rng.below(3) {
        0 => text.to_uppercase(),
        1 => text.to_lowercase(),
        _ => text
            .split(' ')
            .enumerate()
            .map(|(i, w)| if i % 2 == 0 { w.to_uppercase() } else { w.to_lowercase() })
            .collect::<Vec<_>>()
            .join(" "),
    }
}

/// Filler before and after, doubled spaces, a lost full stop: what people actually type. The
/// filler must not change the meaning: "asap" was tried and turned a resting limit order into
/// a question about raising the price, which is the model reading the word correctly.
fn noise(text: &str, rng: &mut Rng) -> String {
    let mut t = text.trim().trim_end_matches('.').to_string();
    if rng.below(2) == 0 {
        t = format!("{}{}", PREFIXES[rng.below(PREFIXES.len() as u64) as usize], t);
    }
    if rng.below(2) == 0 {
        t.push_str(SUFFIXES[rng.below(SUFFIXES.len() as u64) as usize]);
    }
    let words: Vec<&str> = t.split(' ').collect();
    if words.len() > 2 {
        let at = 1 + rng.below(words.len() as u64 - 1) as usize;
        let mut out = words[..at].join(" ");
        out.push_str("  ");
        out.push_str(&words[at..].join(" "));
        return out;
    }
    t
}

/// A word is eligible when it is five or more letters, letters only, and not a word the gate
/// looks for. About one eligible word in three gets two adjacent inner letters swapped, and at
/// least one does when any is eligible.
fn typos(text: &str, rng: &mut Rng) -> String {
    let protected = agent_service::gate::intent_vocabulary();
    let eligible = |w: &str| {
        w.chars().count() >= 5 && w.chars().all(char::is_alphabetic) && !protected.contains(&w.to_lowercase().as_str())
    };
    let mut words: Vec<String> = text.split(' ').map(str::to_string).collect();
    let mut changed = false;
    let mut first_eligible = None;
    for (i, w) in words.iter_mut().enumerate() {
        if !eligible(w) {
            continue;
        }
        first_eligible.get_or_insert(i);
        if rng.below(3) == 0 {
            *w = swap_inner(w, rng);
            changed = true;
        }
    }
    if !changed && let Some(i) = first_eligible {
        words[i] = swap_inner(&words[i], rng);
    }
    words.join(" ")
}

fn swap_inner(word: &str, rng: &mut Rng) -> String {
    let mut chars: Vec<char> = word.chars().collect();
    let at = 1 + rng.below(chars.len() as u64 - 3) as usize;
    chars.swap(at, at + 1);
    chars.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_and_intent_words_survive_and_runs_are_reproducible() {
        let text = "Please cancel my order 17 and then buy 0.5 ETH at 3,000.50 before the weekend.";
        for kind in KINDS {
            let out = apply(kind, text, seed("case", 0, 1)).unwrap();
            assert_eq!(
                out,
                apply(kind, text, seed("case", 0, 1)).unwrap(),
                "{kind} is deterministic"
            );
            let lower = out.to_lowercase();
            for keep in ["17", "0.5", "3,000.50", "cancel", "buy", "eth"] {
                assert!(lower.contains(keep), "{kind} lost {keep}: {out}");
            }
        }
        let typo = apply("typos", text, seed("case", 0, 1)).unwrap();
        assert_ne!(typo, text, "an eligible word gets a typo");
        assert_ne!(
            apply("all", text, seed("case", 0, 1)).unwrap(),
            apply("all", text, seed("case", 0, 2)).unwrap()
        );
        assert!(apply("nonsense", text, 1).is_err());
    }
}
