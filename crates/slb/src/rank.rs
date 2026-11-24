//! Break candidate ranking used to choose where a line breaks.

/// Ranking of a break candidate. A higher rank is a better place to break a line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Rank {
    /// A plain word boundary, only used as a last resort.
    Word,
    /// Before a relative pronoun or a configured extra clause starter.
    ClauseTier4,
    /// Before a subordinating conjunction such as "because" or "while".
    ClauseTier3,
    /// After a comma or dash.
    Punctuation,
    /// Before a coordinating conjunction such as "but" or "or".
    ClauseTier2,
    /// Before "and".
    ClauseTier1,
    /// After a colon introducing an explanation or list.
    Colon,
    /// After the end of a sentence.
    Sentence,
    /// A break the formatter inserted itself, for example after a semicolon rewrite.
    Forced,
}

impl Rank {
    /// The next lower ranking.
    ///
    /// Used to penalize a break candidate whose line does not fit the budget,
    /// so an overflowing break has to be clearly better than one that fits.
    #[must_use]
    pub const fn lowered(self) -> Self {
        match self {
            Self::Word | Self::ClauseTier4 => Self::Word,
            Self::ClauseTier3 => Self::ClauseTier4,
            Self::Punctuation => Self::ClauseTier3,
            Self::ClauseTier2 => Self::Punctuation,
            Self::ClauseTier1 => Self::ClauseTier2,
            Self::Colon => Self::ClauseTier1,
            Self::Sentence => Self::Colon,
            Self::Forced => Self::Sentence,
        }
    }
}

#[cfg(test)]
mod test_rank {
    use super::*;

    #[test]
    fn lowering_moves_one_step_down_and_stops_at_word() {
        assert_eq!(Rank::Forced.lowered(), Rank::Sentence);
        assert_eq!(Rank::Sentence.lowered(), Rank::Colon);
        assert_eq!(Rank::Colon.lowered(), Rank::ClauseTier1);
        assert_eq!(Rank::ClauseTier1.lowered(), Rank::ClauseTier2);
        assert_eq!(Rank::ClauseTier2.lowered(), Rank::Punctuation);
        assert_eq!(Rank::Punctuation.lowered(), Rank::ClauseTier3);
        assert_eq!(Rank::ClauseTier3.lowered(), Rank::ClauseTier4);
        assert_eq!(Rank::ClauseTier4.lowered(), Rank::Word);
        assert_eq!(Rank::Word.lowered(), Rank::Word);
    }

    #[test]
    fn the_ranking_order_is_word_to_forced() {
        assert!(Rank::Word < Rank::ClauseTier4);
        assert!(Rank::ClauseTier4 < Rank::ClauseTier3);
        assert!(Rank::ClauseTier3 < Rank::Punctuation);
        assert!(Rank::Punctuation < Rank::ClauseTier2);
        assert!(Rank::ClauseTier2 < Rank::ClauseTier1);
        assert!(Rank::ClauseTier1 < Rank::Colon);
        assert!(Rank::Colon < Rank::Sentence);
        assert!(Rank::Sentence < Rank::Forced);
    }
}
