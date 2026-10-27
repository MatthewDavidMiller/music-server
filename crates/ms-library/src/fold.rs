//! Folding text for comparison: case, the common Latin diacritics, and
//! runs of whitespace, so "Björk", "BJORK" and "bjork " group, sort, and
//! search as one.

/// The base letter of a Latin-1 or Latin Extended-A character, when it
/// carries a diacritic this folds away.
fn base(c: char) -> Option<&'static str> {
    Some(match c {
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' | 'ă' | 'ą' => "a",
        'æ' => "ae",
        'ç' | 'ć' | 'ĉ' | 'ċ' | 'č' => "c",
        'ď' | 'đ' | 'ð' => "d",
        'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ĕ' | 'ė' | 'ę' | 'ě' => "e",
        'ĝ' | 'ğ' | 'ġ' | 'ģ' => "g",
        'ĥ' | 'ħ' => "h",
        'ì' | 'í' | 'î' | 'ï' | 'ĩ' | 'ī' | 'ĭ' | 'į' | 'ı' => "i",
        'ĵ' => "j",
        'ķ' => "k",
        'ĺ' | 'ļ' | 'ľ' | 'ŀ' | 'ł' => "l",
        'ñ' | 'ń' | 'ņ' | 'ň' => "n",
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' | 'ŏ' | 'ő' => "o",
        'œ' => "oe",
        'ŕ' | 'ŗ' | 'ř' => "r",
        'ś' | 'ŝ' | 'ş' | 'š' => "s",
        'ß' => "ss",
        'ţ' | 'ť' | 'ŧ' => "t",
        'þ' => "th",
        'ù' | 'ú' | 'û' | 'ü' | 'ũ' | 'ū' | 'ŭ' | 'ů' | 'ű' | 'ų' => "u",
        'ŵ' => "w",
        'ý' | 'ÿ' | 'ŷ' => "y",
        'ź' | 'ż' | 'ž' => "z",
        _ => return None,
    })
}

/// Lowercase, without diacritics, with whitespace collapsed to one space.
pub fn fold(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut space = false;
    for c in text.chars().flat_map(char::to_lowercase) {
        if c.is_whitespace() {
            space = !out.is_empty();
            continue;
        }
        if space {
            out.push(' ');
            space = false;
        }
        match base(c) {
            Some(b) => out.push_str(b),
            None => out.push(c),
        }
    }
    out
}

/// Leading articles ignored when sorting and indexing names.
const ARTICLES: &[&str] = &["the ", "a ", "an ", "el ", "la ", "le ", "les ", "los ", "las "];

/// The folded name with a leading article dropped: "The Beatles" sorts as
/// "beatles".
pub fn sort_key(text: &str) -> String {
    let folded = fold(text);
    for a in ARTICLES {
        if let Some(rest) = folded.strip_prefix(a)
            && !rest.is_empty()
        {
            return rest.to_owned();
        }
    }
    folded
}

/// The index letter a name files under: its first sort-key letter, or "#".
pub fn index_letter(text: &str) -> char {
    match sort_key(text).chars().next() {
        Some(c) if c.is_ascii_alphabetic() => c.to_ascii_uppercase(),
        _ => '#',
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folding() {
        assert_eq!(fold("  Björk   Guðmundsdóttir "), "bjork gudmundsdottir");
        assert_eq!(fold("STRASSE"), fold("Straße"));
        assert_eq!(fold("Mötley Crüe"), "motley crue");
        assert_eq!(fold("東京"), "東京");
    }

    #[test]
    fn sorting_and_index() {
        assert_eq!(sort_key("The Beatles"), "beatles");
        assert_eq!(sort_key("The"), "the");
        assert_eq!(sort_key("Los Lobos"), "lobos");
        assert_eq!(index_letter("The Who"), 'W');
        assert_eq!(index_letter("Éric"), 'E');
        assert_eq!(index_letter("2Pac"), '#');
        assert_eq!(index_letter(""), '#');
    }
}
