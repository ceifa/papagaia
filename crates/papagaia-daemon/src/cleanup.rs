//! Local, rule-based cleanup of raw speech transcripts — no LLM, so dictation
//! inserts instantly. Whisper already punctuates well, so this is light polish:
//! literal voice commands ("new line" → break), immediate word-repeat collapse,
//! whitespace tidy-up, and sentence capitalization.
//!
//! Each transform is toggled via [`CleanupConfig`] and conservative by default:
//! filler removal is off (it can change meaning) and capitalization never lowercases.

use std::collections::HashSet;

use papagaia_core::CleanupConfig;

/// Apply the configured cleanup passes to a raw transcript, in a fixed order:
/// voice commands first (they may inject line breaks), then filler removal,
/// dedup, whitespace collapse, and finally capitalization.
pub fn clean(cfg: &CleanupConfig, raw: &str) -> String {
    let mut text = raw.trim().to_string();
    if text.is_empty() {
        return text;
    }

    if cfg.voice_commands {
        text = apply_voice_commands(&text);
    }
    if cfg.remove_fillers {
        let fillers: HashSet<String> = cfg.filler_words.iter().map(|f| f.to_lowercase()).collect();
        text = map_lines(&text, |line| remove_fillers(line, &fillers));
    }
    if cfg.dedupe_repeated_words {
        text = map_lines(&text, dedupe_immediate_repeats);
    }
    if cfg.collapse_whitespace {
        text = collapse_whitespace(&text);
    }
    if cfg.capitalize_sentences {
        text = capitalize_sentence_starts(&text);
    }

    text.trim().to_string()
}

/// A reconstructed output fragment from the voice-command pass.
enum Piece {
    /// A literal word, separated from neighbours by a single space.
    Word(String),
    /// Punctuation that attaches to the preceding text with no leading space.
    Punct(&'static str),
    /// One (`1`) or two (`2`) line breaks.
    Break(u8),
}

/// Normalize a word for matching: lowercase, with leading/trailing punctuation
/// stripped (so "linha." matches "linha"). Returns an empty string for tokens
/// that are pure punctuation.
fn word_key(word: &str) -> String {
    word.trim_matches(|c: char| !c.is_alphanumeric())
        .to_lowercase()
}

/// Resolve a (lowercased, space-joined) phrase to a voice command, if any.
fn command_for(key: &str) -> Option<Piece> {
    Some(match key {
        "new line" | "newline" | "nova linha" | "quebra de linha" => Piece::Break(1),
        "new paragraph" | "novo parágrafo" | "novo paragrafo" => Piece::Break(2),
        "period" | "full stop" | "ponto final" => Piece::Punct("."),
        "comma" | "vírgula" | "virgula" => Piece::Punct(","),
        "question mark" | "ponto de interrogação" | "ponto de interrogacao" => Piece::Punct("?"),
        "exclamation mark" | "exclamation point" | "ponto de exclamação"
        | "ponto de exclamacao" => Piece::Punct("!"),
        "colon" | "dois pontos" => Piece::Punct(":"),
        "semicolon" | "ponto e vírgula" | "ponto e virgula" => Piece::Punct(";"),
        _ => return None,
    })
}

fn apply_voice_commands(text: &str) -> String {
    text.split('\n')
        .map(apply_voice_commands_line)
        .collect::<Vec<_>>()
        .join("\n")
}

fn apply_voice_commands_line(line: &str) -> String {
    let words: Vec<&str> = line.split_whitespace().collect();
    // Normalize each word once up front, then match n-gram windows against the
    // keys (rather than recomputing `word_key` for each overlapping window).
    let keys: Vec<String> = words.iter().map(|w| word_key(w)).collect();
    let mut pieces: Vec<Piece> = Vec::new();
    let mut i = 0;

    while i < words.len() {
        let mut matched = false;
        // Prefer the longest phrase: try 3-word, then 2-word, then 1-word matches.
        for len in (1..=3).rev() {
            if i + len > words.len() || keys[i..i + len].iter().any(String::is_empty) {
                continue;
            }
            if let Some(piece) = command_for(&keys[i..i + len].join(" ")) {
                pieces.push(piece);
                i += len;
                matched = true;
                break;
            }
        }
        if !matched {
            pieces.push(Piece::Word(words[i].to_string()));
            i += 1;
        }
    }

    render_pieces(&pieces)
}

fn render_pieces(pieces: &[Piece]) -> String {
    let mut out = String::new();
    let mut need_space = false;
    for piece in pieces {
        match piece {
            Piece::Word(word) => {
                if need_space {
                    out.push(' ');
                }
                out.push_str(word);
                need_space = true;
            }
            Piece::Punct(punct) => {
                out.push_str(punct);
                need_space = true;
            }
            Piece::Break(count) => {
                for _ in 0..*count {
                    out.push('\n');
                }
                need_space = false;
            }
        }
    }
    out
}

/// Apply a per-line transform while preserving the line structure (single and
/// blank lines), so transforms that reflow words don't eat injected breaks.
fn map_lines(text: &str, transform: impl Fn(&str) -> String) -> String {
    text.split('\n')
        .map(transform)
        .collect::<Vec<_>>()
        .join("\n")
}

fn remove_fillers(line: &str, fillers: &HashSet<String>) -> String {
    line.split_whitespace()
        .filter(|word| {
            let key = word_key(word);
            key.is_empty() || !fillers.contains(&key)
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn dedupe_immediate_repeats(line: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for word in line.split_whitespace() {
        let key = word_key(word);
        if !key.is_empty()
            && out.last().is_some_and(|prev| word_key(prev) == key)
        {
            continue;
        }
        out.push(word);
    }
    out.join(" ")
}

fn collapse_whitespace(text: &str) -> String {
    // Collapse intra-line whitespace and trim each line.
    let collapsed: String = text
        .split('\n')
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect::<Vec<_>>()
        .join("\n");

    // Collapse runs of 3+ newlines down to a paragraph break (two newlines).
    let mut out = String::with_capacity(collapsed.len());
    let mut newlines = 0;
    for ch in collapsed.chars() {
        if ch == '\n' {
            newlines += 1;
            if newlines <= 2 {
                out.push(ch);
            }
        } else {
            newlines = 0;
            out.push(ch);
        }
    }
    out
}

fn capitalize_sentence_starts(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut at_sentence_start = true;
    for ch in text.chars() {
        if at_sentence_start && ch.is_alphabetic() {
            out.extend(ch.to_uppercase());
            at_sentence_start = false;
        } else {
            out.push(ch);
            if matches!(ch, '.' | '?' | '!' | '\n') {
                at_sentence_start = true;
            } else if ch.is_alphanumeric() {
                at_sentence_start = false;
            }
            // Whitespace and other punctuation leave the flag unchanged, so the
            // first letter after a terminator is still capitalized.
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> CleanupConfig {
        CleanupConfig::default()
    }

    #[test]
    fn voice_command_inserts_line_break() {
        let out = clean(&cfg(), "primeira nova linha segunda");
        assert_eq!(out, "Primeira\nSegunda");
    }

    #[test]
    fn voice_command_new_paragraph_inserts_two_breaks() {
        let out = clean(&cfg(), "um texto new paragraph outro texto");
        assert_eq!(out, "Um texto\n\nOutro texto");
    }

    #[test]
    fn voice_command_punctuation_attaches_to_previous_word() {
        let out = clean(&cfg(), "hello period world comma test");
        assert_eq!(out, "Hello. World, test");
    }

    #[test]
    fn longest_phrase_wins() {
        let out = clean(&cfg(), "isso ponto de interrogação certo");
        assert_eq!(out, "Isso? Certo");
    }

    #[test]
    fn dedupe_collapses_immediate_repeats() {
        let out = clean(&cfg(), "I want to to go");
        assert_eq!(out, "I want to go");
    }

    #[test]
    fn capitalizes_each_sentence() {
        let out = clean(&cfg(), "hello there. how are you? fine");
        assert_eq!(out, "Hello there. How are you? Fine");
    }

    #[test]
    fn collapses_extra_whitespace() {
        let out = clean(&cfg(), "a   b\t\tc");
        assert_eq!(out, "A b c");
    }

    #[test]
    fn fillers_kept_by_default() {
        // remove_fillers defaults to false, so "um" survives (just capitalized).
        let out = clean(&cfg(), "um hello world");
        assert_eq!(out, "Um hello world");
    }

    #[test]
    fn fillers_removed_when_enabled() {
        let mut config = cfg();
        config.remove_fillers = true;
        let out = clean(&config, "um hello uh world");
        assert_eq!(out, "Hello world");
    }

    #[test]
    fn filler_removal_never_touches_substrings() {
        let mut config = cfg();
        config.remove_fillers = true;
        config.filler_words = vec!["é".into()];
        // "é" is a filler, but "também" must keep its embedded letters.
        let out = clean(&config, "isso é também importante");
        assert_eq!(out, "Isso também importante");
    }

    #[test]
    fn empty_input_stays_empty() {
        assert_eq!(clean(&cfg(), "   "), "");
    }

    #[test]
    fn disabling_everything_only_trims() {
        let config = CleanupConfig {
            voice_commands: false,
            dedupe_repeated_words: false,
            collapse_whitespace: false,
            capitalize_sentences: false,
            remove_fillers: false,
            filler_words: vec![],
        };
        assert_eq!(clean(&config, "  hello   world  "), "hello   world");
    }
}
