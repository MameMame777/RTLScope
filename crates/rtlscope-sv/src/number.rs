//! Verilog integer literal parsing.
//!
//! Done on the literal's source text rather than by walking the number's
//! syntax subtree: the grammar splits `8'hFF` across half a dozen node types
//! per base, and the text form is both shorter to handle and easier to test.
//!
//! Literals containing `x` or `z` are rejected. They are legal SystemVerilog
//! but have no place in a synthesisable constant, and guessing a value for them
//! is exactly the silent wrongness D2 forbids — the caller turns a `None` here
//! into an `Unsupported` node carrying the original text.

use rtlscope_ir::{ConstBits, UExprKind};

/// Default width for a based literal written without a size, per IEEE 1800.
const UNSIZED_WIDTH: u32 = 32;

/// Parses a Verilog integer literal. Returns `None` for anything outside the
/// subset, including `x`/`z` digits and values too large to represent.
pub fn parse_number(text: &str) -> Option<UExprKind> {
    let cleaned: String = text.chars().filter(|c| !c.is_whitespace() && *c != '_').collect();
    if cleaned.is_empty() {
        return None;
    }

    let Some(tick) = cleaned.find('\'') else {
        // Plain decimal, unsized: `42`.
        return cleaned.parse::<i64>().ok().map(|value| UExprKind::Int { value });
    };

    let (size_text, rest) = cleaned.split_at(tick);
    let rest = &rest[1..];

    // `'0` and `'1` fill to the width of whatever they are assigned to.
    if size_text.is_empty() && (rest == "0" || rest == "1") {
        return Some(UExprKind::Fill { ones: rest == "1" });
    }

    let mut chars = rest.chars();
    let mut base_char = chars.next()?;
    // `8'sd12` — signedness does not change the bit pattern we store.
    if base_char == 's' || base_char == 'S' {
        base_char = chars.next()?;
    }
    let base = match base_char.to_ascii_lowercase() {
        'b' => 2,
        'o' => 8,
        'd' => 10,
        'h' => 16,
        _ => return None,
    };
    let digits: String = chars.collect();
    if digits.is_empty() {
        return None;
    }

    let width = if size_text.is_empty() {
        UNSIZED_WIDTH
    } else {
        size_text.parse::<u32>().ok().filter(|w| *w > 0 && *w <= 1 << 20)?
    };

    let words = digits_to_words(&digits, base)?;
    Some(UExprKind::Sized { value: ConstBits::from_words(width, words) })
}

/// Accumulates digits into little-endian 64-bit words.
fn digits_to_words(digits: &str, base: u32) -> Option<Vec<u64>> {
    if base == 10 {
        let mut acc: u128 = 0;
        for c in digits.chars() {
            let digit = c.to_digit(10)?;
            acc = acc.checked_mul(10)?.checked_add(u128::from(digit))?;
        }
        return Some(vec![acc as u64, (acc >> 64) as u64]);
    }

    let bits_per_digit = base.trailing_zeros();
    // Room for the whole literal, so nothing is lost before the caller masks.
    let word_count = (digits.len() * bits_per_digit as usize).div_ceil(64).max(1);
    let mut words = vec![0u64; word_count];
    for c in digits.chars() {
        let digit = c.to_digit(base)?;
        shift_left(&mut words, bits_per_digit);
        words[0] |= u64::from(digit);
    }
    Some(words)
}

/// Shifts a little-endian word vector left by fewer than 64 bits. Bits carried
/// past the end are dropped; the caller has already sized `words` to fit.
fn shift_left(words: &mut [u64], shift: u32) {
    if shift == 0 {
        return;
    }
    let mut carry = 0u64;
    for word in words.iter_mut() {
        let next_carry = *word >> (64 - shift);
        *word = (*word << shift) | carry;
        carry = next_carry;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sized(text: &str) -> ConstBits {
        match parse_number(text) {
            Some(UExprKind::Sized { value }) => value,
            other => panic!("{text} did not parse as a sized literal: {other:?}"),
        }
    }

    #[test]
    fn plain_decimal_is_unsized() {
        assert_eq!(parse_number("42"), Some(UExprKind::Int { value: 42 }));
        assert_eq!(parse_number("0"), Some(UExprKind::Int { value: 0 }));
    }

    #[test]
    fn based_literals_carry_their_width() {
        let bits = sized("8'hFF");
        assert_eq!(bits.width, 8);
        assert_eq!(bits.to_u64(), Some(0xFF));

        assert_eq!(sized("2'd3").to_u64(), Some(3));
        assert_eq!(sized("1'b1").to_u64(), Some(1));
        assert_eq!(sized("4'o17").to_u64(), Some(0o17));
    }

    #[test]
    fn width_truncates_the_value() {
        // 4'hFF is 4 bits wide; the top nibble is not representable.
        assert_eq!(sized("4'hFF").to_u64(), Some(0xF));
    }

    #[test]
    fn underscores_are_separators() {
        assert_eq!(sized("16'b1010_1010_1010_1010").to_u64(), Some(0xAAAA));
        assert_eq!(parse_number("1_000"), Some(UExprKind::Int { value: 1000 }));
    }

    #[test]
    fn signed_literals_keep_their_bit_pattern() {
        assert_eq!(sized("8'sd12").to_u64(), Some(12));
    }

    #[test]
    fn fill_literals_are_recognised() {
        assert_eq!(parse_number("'0"), Some(UExprKind::Fill { ones: false }));
        assert_eq!(parse_number("'1"), Some(UExprKind::Fill { ones: true }));
    }

    #[test]
    fn unsized_based_literals_default_to_32_bits() {
        assert_eq!(sized("'hFF").width, 32);
    }

    #[test]
    fn literals_wider_than_a_word_are_exact() {
        let bits = sized("128'hDEAD_BEEF_0000_0000_0000_0000_0000_0001");
        assert_eq!(bits.width, 128);
        assert_eq!(bits.words[0], 1);
        assert_eq!(bits.words[1], 0xDEAD_BEEF_0000_0000);
    }

    #[test]
    fn unknown_digits_are_rejected_rather_than_guessed() {
        assert_eq!(parse_number("4'bxx01"), None);
        assert_eq!(parse_number("8'hzz"), None);
        assert_eq!(parse_number("4'b"), None);
        assert_eq!(parse_number("4'q7"), None);
    }
}
