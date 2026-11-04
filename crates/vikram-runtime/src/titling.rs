//! Session naming. A session's title is derived from what the conversation is actually about,
//! once, in the background — never from the session's position in a list.
//!
//! Two halves live here. The prompt and the checks around it: a model is asked for a few words and
//! its answer is treated as untrusted text until it survives [`sanitize`]. And [`fallback`], the
//! deterministic half that runs when no model answered, so a session is never left holding a
//! placeholder because a network failed.

use vikram_contracts::{Message, Role};

/// The starting title. Shown until a real one is derived, and the only name a session with no
/// meaningful message yet ever gets.
pub const NEW_SESSION: &str = "New Session";

/// Longest title we will display or store. Well inside any tab or picker width.
pub const MAX_TITLE_CHARS: usize = 60;

/// A title is a label, not a sentence. Anything past this many words was not following instructions.
const MAX_TITLE_WORDS: usize = 7;

/// How much of the opening message the model is shown. A title needs the topic, not the transcript.
const MAX_SEED_CHARS: usize = 1200;

/// Titles that describe the tool rather than the conversation. A model that was not paying
/// attention produces these, and they are exactly as useless as the placeholders they replace.
const FORBIDDEN: &[&str] = &[
    "general chat",
    "general conversation",
    "general discussion",
    "ai chat",
    "ai conversation",
    "new chat",
    "new conversation",
    "new session",
    "conversation",
    "discussion",
    "session",
    "chat",
    "untitled",
    "hello",
    "hi there",
];

/// Openings that carry no topic at all. A session whose first message is one of these stays
/// unnamed until something worth naming arrives.
const NON_MEANINGFUL: &[&str] = &[
    "hi",
    "hello",
    "hey",
    "yo",
    "howdy",
    "hiya",
    "good morning",
    "good evening",
    "good afternoon",
    "test",
    "testing",
    "asdasd",
    "abc",
    "123",
    "ok",
    "okay",
    "k",
    "yes",
    "no",
    "thanks",
    "thank you",
    "thx",
    "ty",
    "who are you",
    "what can you do",
    "are you there",
    "ping",
    "start",
    "hello?",
    "hi?",
];

/// Words that say nothing about what is being built. Dropped by [`fallback`] when it has to choose
/// the title from raw text without a model.
const STOP_WORDS: &[&str] = &[
    "a", "an", "the", "and", "or", "but", "so", "to", "of", "in", "on", "at", "by", "for", "with",
    "without", "from", "into", "is", "are", "was", "were", "be", "been", "being", "it", "its",
    "this", "that", "these", "those", "i", "you", "we", "they", "he", "she", "my", "your", "our",
    "please", "pls", "can", "could", "would", "should", "will", "shall", "do", "does", "did",
    "make", "want", "need", "some", "any", "about", "how", "what", "why", "when", "where",
];

/// The instruction the title comes back from. Deliberately short and strict: a small model follows
/// a list of rules better than a paragraph of intent, and every token here is spent on the answer.
pub fn prompt(seed: &str) -> String {
    format!(
        "Write a short title for the conversation described below.\n\n\
         Rules:\n\
         - 2 to 7 words.\n\
         - Same language as the message.\n\
         - Name the actual topic, project, task or question.\n\
         - No sentences, no quotes, no trailing period, no emoji, no prefix like \"Title:\".\n\
         - Never answer: General Chat, AI Chat, New Conversation, Session, Untitled.\n\
         - Do not copy the message word for word when it is long; summarise its purpose.\n\n\
         Examples:\n\
         \"Build JWT authentication in Rust\" -> Rust JWT Authentication\n\
         \"research hermes self evolution\" -> Hermes Self-Evolution\n\
         \"the login page is broken after deploy\" -> Login Page Bug Fix\n\
         \"i need a site for my minecraft server hosting\" -> Minecraft Hosting Website\n\
         \"comparar agentes de codigo ia\" -> Comparación de Agentes IA\n\n\
         Message:\n{}\n\n\
         Title:",
        clip(seed, MAX_SEED_CHARS)
    )
}

/// The first message worth naming a session after. Greetings, acknowledgements and punctuation-only
/// openers are skipped, so a session opened with "hi" is titled from what the operator asks next.
pub fn seed(messages: &[Message]) -> Option<String> {
    messages
        .iter()
        .filter(|m| matches!(m.role, Role::User))
        .map(|m| m.text_body())
        .find(|text| is_meaningful(text))
}

/// Whether a message states something a title could be built from.
pub fn is_meaningful(text: &str) -> bool {
    let flat = normalize(text);
    if flat.is_empty() {
        return false;
    }
    // Compared without trailing punctuation, or "Hello!" slips past a list that says "hello".
    let bare = flat.trim_matches(|c: char| !c.is_alphanumeric());
    if NON_MEANINGFUL.contains(&bare) || NON_MEANINGFUL.contains(&flat.as_str()) {
        return false;
    }
    // A single word that is not in the greeting list still rarely names a conversation, but one
    // that has any alphabetic content might ("Photoshop"), so length and letters decide.
    flat.chars().filter(|c| c.is_alphabetic()).count() >= 3
        // "ok thanks for the help" is still an acknowledgement, not a request.
        && !flat.split_whitespace().all(|w| {
            let w = w.trim_matches(|c: char| !c.is_alphanumeric());
            STOP_WORDS.contains(&w) || NON_MEANINGFUL.contains(&w)
        })
}

/// Turn what a model returned into a title we are willing to show, or nothing at all. Model output
/// is content, never a decision: this is where a rambling, quoted, empty or deliberately
/// placeholder-ish answer is rejected rather than trusted.
pub fn sanitize(raw: &str) -> Option<String> {
    // Only the first line: a model that ignores instructions usually explains itself underneath.
    let line = raw.lines().next().unwrap_or_default();
    let mut text = line.trim().to_owned();

    // Strip wrappers a model adds despite being told not to.
    for (open, close) in [
        ("\"", "\""),
        ("'", "'"),
        ("“", "”"),
        ("«", "»"),
        ("`", "`"),
        ("[", "]"),
        ("(", ")"),
    ] {
        text = text
            .trim()
            .trim_start_matches(open)
            .trim_end_matches(close)
            .trim()
            .to_owned();
    }
    // Bullets and markdown emphasis, then the label prefixes a chatty model appends.
    text = text
        .trim_start_matches('#')
        .trim_start_matches('*')
        .trim_start_matches('-')
        .trim()
        .to_owned();
    for prefix in [
        "title:",
        "session title:",
        "conversation title:",
        "name:",
        "this conversation is about",
        "the title is",
        "suggested title:",
    ] {
        let lower = text.to_lowercase();
        if lower.starts_with(prefix) {
            text[0..prefix.len()].make_ascii_lowercase();
            text = text[prefix.len()..]
                .trim_start_matches([':', '-', ' '])
                .to_owned();
        }
    }
    // Kill any control characters and collapse whitespace before measuring.
    text = text
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    // Trailing punctuation is decoration on a label.
    while text.ends_with(['.', ':', '!', '?', ';', ',']) {
        text.pop();
        text = text.trim().to_owned();
    }
    if text.chars().filter(|c| c.is_alphabetic()).count() < 2 {
        return None;
    }
    // Cap word count by cutting at a word boundary rather than mid-word.
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.len() > MAX_TITLE_WORDS {
        text = words[..MAX_TITLE_WORDS].join(" ");
    }
    let flat = normalize(&text);
    let bare = flat.trim_matches(|c: char| !c.is_alphanumeric());
    if FORBIDDEN.contains(&flat.as_str())
        || FORBIDDEN.contains(&bare)
        || NON_MEANINGFUL.contains(&bare)
        || is_placeholder(&text)
    {
        return None;
    }
    text = cap_words(&clip(&text, MAX_TITLE_CHARS));
    if text.chars().filter(|c| c.is_alphabetic()).count() < 2 {
        return None;
    }
    Some(text)
}

/// A session's default title carries no information. Used to decide whether a name is worth keeping.
pub fn is_placeholder(title: &str) -> bool {
    let t = title.trim();
    if t.eq_ignore_ascii_case(NEW_SESSION) || t.eq_ignore_ascii_case("session") {
        return true;
    }
    match t.strip_prefix("Session ") {
        Some(rest) => !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()),
        None => false,
    }
}

/// The deterministic title: used when no model is configured, when the request failed, or when the
/// answer was unusable. Built from the same message the model was shown, so it still says what the
/// session is about — just in plainer words than the model would have chosen.
pub fn fallback(source: &str) -> String {
    let first_sentence = source
        .split(['.', '!', '?', '\n'])
        .map(str::trim)
        .find(|part| is_meaningful(part))
        .unwrap_or(source.trim());
    let mut kept: Vec<String> = Vec::new();
    for word in first_sentence.split_whitespace() {
        let bare = word.trim_matches(|c: char| !c.is_alphanumeric() && c != '-');
        if bare.is_empty() {
            continue;
        }
        // Leading filler ("ok so can you build") is dropped until something with content arrives.
        if kept.is_empty()
            && (STOP_WORDS.contains(&bare.to_lowercase().as_str())
                || NON_MEANINGFUL.contains(&bare.to_lowercase().as_str()))
        {
            continue;
        }
        kept.push(word.to_owned());
        if kept.len() >= 6 {
            break;
        }
    }
    // A label must not end on a dangling connector: "Build JWT authentication in Rust for" reads as
    // a mistake, and it is exactly what a six-word cut produces.
    while kept.last().is_some_and(|last| {
        let bare = last
            .trim_matches(|c: char| !c.is_alphanumeric() && c != '-')
            .to_lowercase();
        STOP_WORDS.contains(&bare.as_str())
    }) {
        kept.pop();
    }
    let title = kept.join(" ");
    sanitize(&title).unwrap_or_else(|| clip(&title, MAX_TITLE_CHARS))
}

/// Collapse to a single line of lowercase words, for comparison only.
fn normalize(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Truncate to `max` characters without cutting a word in half, marking the cut the way the rest of
/// the interface does.
pub fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let head: String = text.chars().take(max).collect();
    match head.rfind(' ') {
        Some(space) if space > max / 2 => head[..space].trim_end().to_owned(),
        _ => head.trim_end().to_owned(),
    }
}

/// Re-apply the word cap after any character clipping, so a truncated tail cannot leave a runt word.
fn cap_words(text: &str) -> String {
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.len() > MAX_TITLE_WORDS {
        words[..MAX_TITLE_WORDS].join(" ")
    } else {
        text.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vikram_contracts::Trust;

    fn user(text: &str) -> Message {
        Message::text(Role::User, text.to_owned(), Trust::User)
    }

    #[test]
    fn greetings_are_no_seed_so_the_session_waits_for_a_real_request() {
        for greeting in [
            "hi",
            "Hello!",
            "thanks",
            "ok",
            "asdasd",
            "?",
            "good morning",
        ] {
            assert_eq!(seed(&[user(greeting)]), None, "must skip {greeting:?}");
        }
        assert_eq!(
            seed(&[user("hi"), user("build a jwt middleware")]).as_deref(),
            Some("build a jwt middleware")
        );
    }

    #[test]
    fn a_model_answer_is_cleaned_into_a_label() {
        assert_eq!(
            sanitize("\"Rust JWT Authentication\"").as_deref(),
            Some("Rust JWT Authentication")
        );
        assert_eq!(
            sanitize("Title: Hermes Self-Evolution\n\nThis covers the evolution engine.")
                .as_deref(),
            Some("Hermes Self-Evolution")
        );
        assert_eq!(
            sanitize("  Login Page Bug Fix.  ").as_deref(),
            Some("Login Page Bug Fix")
        );
    }

    #[test]
    fn a_model_that_ignores_instructions_is_refused_not_stored() {
        for bad in [
            "General Chat",
            "AI Chat",
            "New Conversation",
            "",
            "…",
            "42",
            "Session 3",
            "New Session",
            "ok",
        ] {
            assert_eq!(sanitize(bad), None, "must refuse {bad:?}");
        }
    }

    #[test]
    fn an_overlong_answer_is_cut_at_a_word_boundary_not_mid_word() {
        let long = sanitize(
            "we should probably refactor the entire authentication module and add refresh tokens too",
        );
        assert_eq!(
            long.as_deref(),
            Some("we should probably refactor the entire authentication")
        );
        assert_eq!(long.as_deref().unwrap().split_whitespace().count(), 7);
    }

    #[test]
    fn a_foreign_language_title_is_kept_as_the_model_wrote_it() {
        assert_eq!(
            sanitize("Comparación de Agentes IA").as_deref(),
            Some("Comparación de Agentes IA")
        );
    }

    #[test]
    fn fallback_names_the_request_without_a_model() {
        assert_eq!(
            fallback("Build JWT authentication in Rust for my api"),
            "Build JWT authentication in Rust"
        );
        assert_eq!(
            fallback("ok so can you fix the login page please"),
            "fix the login page"
        );
        // Nothing usable: the caller keeps the placeholder rather than inventing a title.
        assert!(fallback("???").is_empty() || is_placeholder(&fallback("???")));
    }

    #[test]
    fn the_placeholder_is_recognised_but_a_real_title_is_not() {
        assert!(is_placeholder("New Session"));
        assert!(is_placeholder("Session 12"));
        assert!(!is_placeholder("Session 1a"));
        assert!(!is_placeholder("Rust JWT Authentication"));
    }

    #[test]
    fn a_title_never_exceeds_the_display_cap() {
        let long = "one two three four five six seven eight nine ten eleven twelve thirteen";
        let out = sanitize(long).unwrap();
        assert!(out.chars().count() <= MAX_TITLE_CHARS, "{out}");
        assert!(out.split_whitespace().count() <= MAX_TITLE_WORDS, "{out}");
    }
}
