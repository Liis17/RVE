//! Границы графем для правки InputField.
//!
//! Курсор поля ввода хранится как байтовое смещение на границе расширенной графемы
//! (UAX #29): буква с комбинируемыми знаками и ZWJ-эмодзи — одна позиция курсора.
//! Отрисовка epaint ожидает индекс в символах, поэтому позиция переводится в
//! индекс показанного текста функцией [`caret_index`].

use crate::document::ContentType;
use unicode_segmentation::UnicodeSegmentation;

/// Число графем — длина текста в смысле `max_length`.
pub(crate) fn grapheme_count(text: &str) -> usize {
    text.graphemes(true).count()
}

/// Граница графемы перед `at` или 0.
pub(crate) fn prev_boundary(text: &str, at: usize) -> usize {
    text.grapheme_indices(true)
        .map(|(start, _)| start)
        .take_while(|&start| start < at)
        .last()
        .unwrap_or(0)
}

/// Граница графемы после `at` или длина текста.
pub(crate) fn next_boundary(text: &str, at: usize) -> usize {
    text.grapheme_indices(true)
        .map(|(start, grapheme)| start + grapheme.len())
        .find(|&end| end > at)
        .unwrap_or(text.len())
}

/// Ближайшая граница графемы не левее `at`; `at` за концом текста ограничивается длиной.
pub(crate) fn snap_boundary(text: &str, at: usize) -> usize {
    if at == 0 {
        return 0;
    }
    next_boundary(text, at.min(text.len()).saturating_sub(1))
}

/// Индекс символа показанного текста для курсора на байте `at`: в пароле каждая графема
/// показана одним знаком маски.
pub(crate) fn caret_index(content: ContentType, text: &str, at: usize) -> usize {
    let before = &text[..snap_boundary(text, at)];
    match content {
        ContentType::Password => grapheme_count(before),
        ContentType::Standard | ContentType::Integer | ContentType::Decimal => {
            before.chars().count()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const E_ACUTE: &str = "e\u{301}";
    const FAMILY: &str = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}";

    /// Все позиции курсора слева направо и справа налево.
    fn walk(text: &str) -> (Vec<usize>, Vec<usize>) {
        let mut forward = vec![0];
        while *forward.last().unwrap() < text.len() {
            forward.push(next_boundary(text, *forward.last().unwrap()));
        }
        let mut backward = vec![text.len()];
        while *backward.last().unwrap() > 0 {
            backward.push(prev_boundary(text, *backward.last().unwrap()));
        }
        (forward, backward)
    }

    #[test]
    fn boundaries_follow_graphemes_for_all_scripts() {
        let cases = [
            ("abc", vec![0, 1, 2, 3]),
            ("при", vec![0, 2, 4, 6]),
            (&*format!("a{E_ACUTE}b"), vec![0, 1, 4, 5]),
            (
                &*format!("x{FAMILY}y"),
                vec![0, 1, 1 + FAMILY.len(), 2 + FAMILY.len()],
            ),
            ("", vec![0]),
        ];
        for (text, expected) in cases {
            let (forward, mut backward) = walk(text);
            assert_eq!(forward, expected, "{text:?}");
            backward.reverse();
            assert_eq!(backward, expected, "{text:?}");
        }
    }

    #[test]
    fn counts_graphemes_not_chars() {
        assert_eq!(grapheme_count("abc"), 3);
        assert_eq!(grapheme_count("кот"), 3);
        assert_eq!(grapheme_count(E_ACUTE), 1);
        assert_eq!(grapheme_count(FAMILY), 1);
        assert_eq!(grapheme_count(""), 0);
    }

    #[test]
    fn snap_moves_inside_grapheme_to_its_end() {
        let text = format!("a{FAMILY}");
        assert_eq!(snap_boundary(&text, 0), 0);
        assert_eq!(snap_boundary(&text, 1), 1);
        assert_eq!(snap_boundary(&text, 5), text.len());
        assert_eq!(snap_boundary(&text, 999), text.len());
        assert_eq!(snap_boundary(E_ACUTE, 1), E_ACUTE.len());
    }

    #[test]
    fn caret_index_matches_shown_text() {
        let text = format!("я{E_ACUTE}{FAMILY}");
        let end = text.len();
        assert_eq!(caret_index(ContentType::Standard, &text, 0), 0);
        assert_eq!(caret_index(ContentType::Standard, &text, 2), 1);
        assert_eq!(
            caret_index(ContentType::Standard, &text, 2 + E_ACUTE.len()),
            3
        );
        assert_eq!(caret_index(ContentType::Standard, &text, end), 8);
        assert_eq!(caret_index(ContentType::Password, &text, end), 3);
        assert_eq!(caret_index(ContentType::Password, &text, 2), 1);
    }
}
