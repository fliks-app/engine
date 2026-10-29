//! Language and flag inference from stream titles. The host uses JS regexes whose `\b` is
//! ASCII-only; they are matched here by a tiny word matcher instead of a regex dependency.
use crate::jsnum::is_space;

/// `\w` of a JS regex: `\b` is ASCII-only there.
fn is_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

enum Elem {
    Lit(char),
    Class(Vec<char>),
    /// `[\s._-]`
    Sep,
}

/// Pattern language: literals, `[abc]`, `~` (separator class), `?` on the previous element.
fn parse(p: &str) -> Vec<(Elem, bool)> {
    let mut out: Vec<(Elem, bool)> = vec![];
    let mut it = p.chars();
    while let Some(c) = it.next() {
        match c {
            '[' => out.push((
                Elem::Class(it.by_ref().take_while(|&c| c != ']').collect()),
                false,
            )),
            '~' => out.push((Elem::Sep, false)),
            '?' => {
                if let Some(last) = out.last_mut() {
                    last.1 = true;
                }
            }
            c => out.push((Elem::Lit(c), false)),
        }
    }
    out
}

fn eats(e: &Elem, c: char) -> bool {
    match e {
        Elem::Lit(l) => *l == c,
        Elem::Class(set) => set.contains(&c),
        Elem::Sep => is_space(c) || matches!(c, '.' | '_' | '-'),
    }
}

fn boundary(t: &[char], i: usize) -> bool {
    (i > 0 && is_word(t[i - 1])) != (i < t.len() && is_word(t[i]))
}

fn ends_at(pat: &[(Elem, bool)], t: &[char], pos: usize) -> bool {
    match pat.split_first() {
        None => boundary(t, pos),
        Some(((e, optional), rest)) => {
            (pos < t.len() && eats(e, t[pos]) && ends_at(rest, t, pos + 1))
                || (*optional && ends_at(rest, t, pos))
        }
    }
}

/// `\b(alt|alt|...)\b` found anywhere in `t`, at a position `skip` does not reject.
fn find_word(t: &[char], alts: &[&str], skip: impl Fn(&[char], usize) -> bool) -> bool {
    let pats: Vec<_> = alts.iter().map(|a| parse(a)).collect();
    (0..t.len()).any(|i| boundary(t, i) && !skip(t, i) && pats.iter().any(|p| ends_at(p, t, i)))
}

fn no_skip(_: &[char], _: usize) -> bool {
    false
}

const LANGUAGES: &[(&str, &[&str])] = &[
    (
        "fre",
        &[
            "french",
            "francais",
            "fran[cç]ais",
            "vff",
            "vfi",
            "vf",
            "truefrench",
            "vostfr",
            "vost",
        ],
    ),
    ("eng", &["english", "anglais", "eng"]),
    ("ger", &["german", "deutsch", "allemand", "ger", "deu"]),
    (
        "spa",
        &[
            "spanish",
            "espanol",
            "espa[nñ]ol",
            "espagnol",
            "esp",
            "spa",
            "latino",
        ],
    ),
    ("ita", &["italian", "italiano", "italien", "ita"]),
    (
        "por",
        &[
            "portuguese",
            "portugu[eê]s",
            "portugais",
            "pt[- ]br",
            "ptbr",
            "por",
        ],
    ),
    ("jpn", &["japanese", "japon[ae]s", "japonais", "jap", "jpn"]),
    ("kor", &["korean", "cor[eé]en", "kor"]),
    (
        "chi",
        &["chinese", "chinois", "chi", "chn", "mandarin", "cantonese"],
    ),
    ("rus", &["russian", "russe", "rus"]),
    ("ara", &["arabic", "arabe", "ara"]),
    (
        "dut",
        &[
            "dutch",
            "n[ée]erlandais",
            "n[ée]er",
            "flemish",
            "nld",
            "dut",
        ],
    ),
    ("pol", &["polish", "polonais", "pol"]),
    ("tur", &["turkish", "turc", "tur"]),
    ("swe", &["swedish", "su[eè]dois", "swe"]),
    ("dan", &["danish", "danois", "dan"]),
    ("nor", &["norwegian", "norv[eé]gien", "nor"]),
    ("fin", &["finnish", "finlandais", "fin"]),
    ("hin", &["hindi", "hin"]),
    ("cze", &["czech", "tch[eè]que", "ces", "cze"]),
    ("rum", &["romanian", "roumain", "ron", "rum"]),
    ("hun", &["hungarian", "hongrois", "hun"]),
    ("tha", &["thai", "thai[ée]?landais", "tha"]),
    ("vie", &["vietnamese", "vietnamien", "vie"]),
    ("heb", &["hebrew", "h[eé]breu", "heb"]),
    ("gre", &["greek", "grec", "ell", "gre"]),
    ("ukr", &["ukrainian", "ukrainien", "ukr"]),
    ("ind", &["indonesian", "indon[eé]sien", "ind"]),
];

/// `inferLanguageCodeFromTitle`: the ISO 639-2/B code a title names, if any.
pub fn language_from_title(title: Option<&str>) -> Option<&'static str> {
    let title = title.filter(|t| !t.is_empty())?;
    let t: Vec<char> = title
        .chars()
        .map(|c| if matches!(c, '.' | '_' | '-') { ' ' } else { c })
        .collect::<String>()
        .to_lowercase()
        .chars()
        .collect();
    LANGUAGES
        .iter()
        .find(|(_, alts)| find_word(&t, alts, no_skip))
        .map(|(code, _)| *code)
}

/// `subtitleFlagsFromTitle`: (forced, hearing impaired) a muxer wrote only in the title.
pub fn subtitle_flags_from_title(title: Option<&str>) -> (bool, bool) {
    let t: Vec<char> = title.unwrap_or("").to_ascii_lowercase().chars().collect();
    // (?<!non[\s._-])(?<!not\s)
    let negated = |t: &[char], i: usize| {
        i >= 4
            && (t[i - 4..i - 1] == ['n', 'o', 'n'] && eats(&Elem::Sep, t[i - 1])
                || t[i - 4..i - 1] == ['n', 'o', 't'] && is_space(t[i - 1]))
    };
    (
        find_word(&t, &["forced", "foreign~?parts?"], negated),
        find_word(
            &t,
            &["sdh", "cc", "hearing~?impaired", "closed~?captions?"],
            no_skip,
        ),
    )
}

/// `isImageBasedSubtitleCodec`: bitmap subtitles that need burn-in.
pub fn is_image_based(codec: Option<&str>) -> bool {
    matches!(
        codec,
        Some("hdmv_pgs_subtitle" | "dvd_subtitle" | "dvb_subtitle" | "xsub")
    )
}

/// Codecs ffmpeg can convert to ASS: not bitmaps, and not the text-like ones with no usable decoder
/// (teletext decodes to bitmaps, TTML and TextST have none) or an unknown codec.
pub fn converts_to_ass(codec: Option<&str>) -> bool {
    !is_image_based(codec)
        && !matches!(
            codec,
            None | Some("" | "dvb_teletext" | "ttml" | "hdmv_text_subtitle")
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn languages() {
        assert_eq!(language_from_title(Some("French AC3 5.1")), Some("fre"));
        assert_eq!(language_from_title(Some("Fran\u{e7}ais")), Some("fre"));
        assert_eq!(language_from_title(Some("pt-BR")), Some("por"));
        assert_eq!(language_from_title(Some("Thai\u{e9}landais")), Some("tha"));
        assert_eq!(language_from_title(Some("Commentary")), None);
        assert_eq!(language_from_title(Some("")), None);
        assert_eq!(language_from_title(Some("seng")), None);
    }

    #[test]
    fn flags() {
        assert_eq!(subtitle_flags_from_title(Some("Forced")), (true, false));
        assert_eq!(
            subtitle_flags_from_title(Some("non-forced")),
            (false, false)
        );
        assert_eq!(
            subtitle_flags_from_title(Some("not forced")),
            (false, false)
        );
        assert_eq!(
            subtitle_flags_from_title(Some("English SDH")),
            (false, true)
        );
        assert_eq!(
            subtitle_flags_from_title(Some("Foreign Parts only")),
            (true, false)
        );
        assert_eq!(
            subtitle_flags_from_title(Some("Closed Captions")),
            (false, true)
        );
        assert_eq!(subtitle_flags_from_title(None), (false, false));
    }
}
