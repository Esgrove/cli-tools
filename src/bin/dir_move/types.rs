pub use cli_tools::dir_move::types::*;

#[cfg(test)]
mod test_casing_score {
    use super::*;

    #[test]
    fn camel_case_scores_highest() {
        // CamelCase (starts uppercase, has both upper and lower) = 3
        assert_eq!(PrefixGroupBuilder::casing_score("PhotoLab"), 3);
        assert_eq!(PrefixGroupBuilder::casing_score("NeonLight"), 3);
        assert_eq!(PrefixGroupBuilder::casing_score("MyApp"), 3);
    }

    #[test]
    fn all_uppercase_scores_second() {
        // Starts uppercase, all same case = 2
        assert_eq!(PrefixGroupBuilder::casing_score("PHOTOLAB"), 2);
        assert_eq!(PrefixGroupBuilder::casing_score("NEONLIGHT"), 2);
        assert_eq!(PrefixGroupBuilder::casing_score("ABC"), 2);
    }

    #[test]
    fn mixed_case_starting_lowercase_scores_third() {
        // Mixed case but starts lowercase = 1
        assert_eq!(PrefixGroupBuilder::casing_score("photoLab"), 1);
        assert_eq!(PrefixGroupBuilder::casing_score("neonLight"), 1);
    }

    #[test]
    fn all_lowercase_scores_zero() {
        // All lowercase = 0
        assert_eq!(PrefixGroupBuilder::casing_score("photolab"), 0);
        assert_eq!(PrefixGroupBuilder::casing_score("neonlight"), 0);
    }

    #[test]
    fn empty_string_scores_zero() {
        assert_eq!(PrefixGroupBuilder::casing_score(""), 0);
    }
}

#[cfg(test)]
mod test_is_better_prefix {
    use super::*;

    #[test]
    fn camel_case_preferred_over_all_uppercase() {
        assert!(PrefixGroupBuilder::is_better_prefix("PhotoLab", "PHOTOLAB"));
        assert!(!PrefixGroupBuilder::is_better_prefix("PHOTOLAB", "PhotoLab"));
    }

    #[test]
    fn camel_case_preferred_over_all_lowercase() {
        assert!(PrefixGroupBuilder::is_better_prefix("PhotoLab", "photolab"));
        assert!(!PrefixGroupBuilder::is_better_prefix("photolab", "PhotoLab"));
    }

    #[test]
    fn all_uppercase_preferred_over_all_lowercase() {
        assert!(PrefixGroupBuilder::is_better_prefix("PHOTOLAB", "photolab"));
        assert!(!PrefixGroupBuilder::is_better_prefix("photolab", "PHOTOLAB"));
    }

    #[test]
    fn alphabetical_order_breaks_ties() {
        // Both CamelCase - alphabetical order wins
        assert!(PrefixGroupBuilder::is_better_prefix("NeonLight", "PhotoLab"));
        assert!(!PrefixGroupBuilder::is_better_prefix("PhotoLab", "NeonLight"));

        // Both all lowercase - alphabetical order wins
        assert!(PrefixGroupBuilder::is_better_prefix("abc", "xyz"));
        assert!(!PrefixGroupBuilder::is_better_prefix("xyz", "abc"));
    }

    #[test]
    fn same_prefix_not_better() {
        assert!(!PrefixGroupBuilder::is_better_prefix("PhotoLab", "PhotoLab"));
    }
}

#[cfg(test)]
mod test_filtered_parts_new {
    use super::*;

    #[test]
    fn single_parts_split_correctly() {
        let parts = FilteredParts::new("Photo.Lab.Image.jpg");
        assert_eq!(parts.parts_original, ["Photo", "Lab", "Image", "jpg"]);
        assert_eq!(parts.parts_lower, ["photo", "lab", "image", "jpg"]);
    }

    #[test]
    fn two_part_combinations_computed() {
        let parts = FilteredParts::new("Photo.Lab.Image");
        assert_eq!(parts.two_parts_lower, ["photolab", "labimage"]);
        assert_eq!(parts.two_parts_original, ["PhotoLab", "LabImage"]);
    }

    #[test]
    fn three_part_combinations_computed() {
        let parts = FilteredParts::new("Photo.Lab.Image.Extra");
        assert_eq!(parts.three_parts_lower, ["photolabimage", "labimageextra"]);
        assert_eq!(parts.three_parts_original, ["PhotoLabImage", "LabImageExtra"]);
    }

    #[test]
    fn single_part_name_has_no_combinations() {
        let parts = FilteredParts::new("standalone");
        assert_eq!(parts.parts_original, ["standalone"]);
        assert_eq!(parts.parts_lower, ["standalone"]);
        assert!(parts.two_parts_lower.is_empty());
        assert!(parts.three_parts_lower.is_empty());
        assert!(parts.two_parts_original.is_empty());
        assert!(parts.three_parts_original.is_empty());
    }

    #[test]
    fn two_part_name_has_no_three_part_combinations() {
        let parts = FilteredParts::new("Photo.Lab");
        assert_eq!(parts.two_parts_lower, ["photolab"]);
        assert_eq!(parts.two_parts_original, ["PhotoLab"]);
        assert!(parts.three_parts_lower.is_empty());
        assert!(parts.three_parts_original.is_empty());
    }

    #[test]
    fn preserves_original_casing() {
        let parts = FilteredParts::new("PhotoLab.ImagePRO.Test");
        assert_eq!(parts.parts_original, ["PhotoLab", "ImagePRO", "Test"]);
        assert_eq!(parts.two_parts_original, ["PhotoLabImagePRO", "ImagePROTest"]);
        assert_eq!(parts.three_parts_original, ["PhotoLabImagePROTest"]);
    }

    #[test]
    fn lowercased_parts_are_consistent_with_original() {
        let parts = FilteredParts::new("UPPER.Mixed.lower");
        assert_eq!(parts.parts_lower, ["upper", "mixed", "lower"]);
        assert_eq!(parts.two_parts_lower, ["uppermixed", "mixedlower"]);
        assert_eq!(parts.three_parts_lower, ["uppermixedlower"]);
    }

    #[test]
    fn empty_string_produces_single_empty_part() {
        let parts = FilteredParts::new("");
        assert_eq!(parts.parts_original, [""]);
        assert_eq!(parts.parts_lower, [""]);
        assert!(parts.two_parts_lower.is_empty());
        assert!(parts.three_parts_lower.is_empty());
    }

    #[test]
    fn many_parts_produce_correct_combination_counts() {
        let parts = FilteredParts::new("A.B.C.D.E");
        assert_eq!(parts.parts_original.len(), 5);
        assert_eq!(parts.two_parts_original.len(), 4);
        assert_eq!(parts.three_parts_original.len(), 3);
    }
}

#[cfg(test)]
mod test_filtered_parts_prefix_matches {
    use super::*;

    #[test]
    fn single_part_exact_match() {
        let parts = FilteredParts::new("PhotoLab.Image.jpg");
        assert!(parts.prefix_matches_normalized("photolab"));
    }

    #[test]
    fn single_part_exact_match_case_insensitive() {
        let parts = FilteredParts::new("PHOTOLAB.Image.jpg");
        assert!(parts.prefix_matches_normalized("photolab"));
    }

    #[test]
    fn two_part_combined_exact_match() {
        let parts = FilteredParts::new("Photo.Lab.Image.jpg");
        assert!(parts.prefix_matches_normalized("photolab"));
    }

    #[test]
    fn three_part_combined_exact_match() {
        let parts = FilteredParts::new("Ph.oto.Lab.Image.jpg");
        assert!(parts.prefix_matches_normalized("photolab"));
    }

    #[test]
    fn no_match_returns_false() {
        let parts = FilteredParts::new("Other.Album.jpg");
        assert!(!parts.prefix_matches_normalized("photolab"));
    }

    #[test]
    fn match_at_middle_position() {
        let parts = FilteredParts::new("Extra.StudioName.video.mp4");
        assert!(parts.prefix_matches_normalized("studioname"));
    }

    #[test]
    fn two_part_match_at_middle_position() {
        let parts = FilteredParts::new("Extra.Photo.Lab.video.mp4");
        assert!(parts.prefix_matches_normalized("photolab"));
    }

    #[test]
    fn starts_with_at_word_boundary_uppercase() {
        // "PhotoLabTV" — 'T' after "PhotoLab" is uppercase, valid boundary
        let parts = FilteredParts::new("PhotoLabTV.Image.jpg");
        assert!(parts.prefix_matches_normalized("photolab"));
    }

    #[test]
    fn starts_with_rejected_at_lowercase_continuation() {
        // "PhotoLabs" — 's' after "PhotoLab" is lowercase, NOT a boundary
        let parts = FilteredParts::new("PhotoLabs.Image.jpg");
        assert!(!parts.prefix_matches_normalized("photolab"));
    }

    #[test]
    fn starts_with_at_digit_boundary() {
        // "Studio2" — '2' after "Studio" is a digit following a letter, valid boundary
        let parts = FilteredParts::new("Studio2.Video.mp4");
        assert!(parts.prefix_matches_normalized("studio"));
    }

    #[test]
    fn two_part_combined_starts_with_at_word_boundary() {
        // "Photo.LabPro" combined is "PhotoLabPro", starts with "photolab" at uppercase 'P'
        let parts = FilteredParts::new("Photo.LabPro.Image.jpg");
        assert!(parts.prefix_matches_normalized("photolab"));
    }

    #[test]
    fn two_part_combined_starts_with_rejected_at_lowercase() {
        // "Photo.Labs" combined is "PhotoLabs", starts with "photolab" but 's' is lowercase
        let parts = FilteredParts::new("Photo.Labs.Image.jpg");
        assert!(!parts.prefix_matches_normalized("photolab"));
    }

    #[test]
    fn three_part_combined_starts_with_at_word_boundary() {
        // "Al.pha.BetaGamma" combined is "AlphaBetaGamma", starts with "alphabeta" at uppercase 'G'
        let parts = FilteredParts::new("Al.pha.BetaGamma.video.mp4");
        assert!(parts.prefix_matches_normalized("alphabeta"));
    }

    #[test]
    fn three_part_combined_starts_with_rejected_at_lowercase() {
        // "Al.pha.Betas" combined is "AlphaBetas", starts with "alphabeta" but 's' is lowercase
        let parts = FilteredParts::new("Al.pha.Betas.video.mp4");
        assert!(!parts.prefix_matches_normalized("alphabeta"));
    }

    #[test]
    fn single_part_file_exact_match() {
        let parts = FilteredParts::new("standalone");
        assert!(parts.prefix_matches_normalized("standalone"));
    }

    #[test]
    fn single_part_file_no_match() {
        let parts = FilteredParts::new("standalone");
        assert!(!parts.prefix_matches_normalized("other"));
    }

    #[test]
    fn empty_target_matches_nothing() {
        let parts = FilteredParts::new("Some.File.mp4");
        assert!(!parts.prefix_matches_normalized(""));
    }

    #[test]
    fn prefix_not_at_start_of_any_part() {
        // "XPhotoLab" does not start with "photolab" — "x" comes first
        let parts = FilteredParts::new("XPhotoLab.Image.jpg");
        assert!(!parts.prefix_matches_normalized("photolab"));
    }

    #[test]
    fn all_uppercase_name_with_word_boundary() {
        // "PHOTOLABPRO" — all uppercase, boundary at position 8 sees 'P' (uppercase) → match
        let parts = FilteredParts::new("PHOTOLABPRO.Image.jpg");
        assert!(parts.prefix_matches_normalized("photolab"));
    }

    #[test]
    fn intense_not_matched_by_intensely() {
        let parts = FilteredParts::new("Intensely.Video.001.mp4");
        assert!(!parts.prefix_matches_normalized("intense"));
    }

    #[test]
    fn intense_exact_match() {
        let parts = FilteredParts::new("Intense.Video.001.mp4");
        assert!(parts.prefix_matches_normalized("intense"));
    }

    #[test]
    fn scandic_single_part_exact_match() {
        let parts = FilteredParts::new("Hälso.Video.001.mp4");
        assert!(parts.prefix_matches_normalized("hälso"));
    }

    #[test]
    fn scandic_starts_with_uppercase_boundary() {
        // "HälsoCenter" starts with "hälso" and 'C' is uppercase → valid boundary
        let parts = FilteredParts::new("HälsoCenter.Video.001.mp4");
        assert!(parts.prefix_matches_normalized("hälso"));
    }

    #[test]
    fn scandic_starts_with_lowercase_rejected() {
        // "Hälsosam" starts with "hälso" but 's' is lowercase → not a boundary
        let parts = FilteredParts::new("Hälsosam.Video.001.mp4");
        assert!(!parts.prefix_matches_normalized("hälso"));
    }

    #[test]
    fn scandic_starts_with_scandic_uppercase_boundary() {
        // "HälsoÖversikt" starts with "hälso" and 'Ö' is uppercase → valid boundary
        let parts = FilteredParts::new("HälsoÖversikt.Video.001.mp4");
        assert!(parts.prefix_matches_normalized("hälso"));
    }

    #[test]
    fn scandic_starts_with_scandic_lowercase_rejected() {
        // "Hälsoöversikt" starts with "hälso" but 'ö' is lowercase → not a boundary
        let parts = FilteredParts::new("Hälsoöversikt.Video.001.mp4");
        assert!(!parts.prefix_matches_normalized("hälso"));
    }

    #[test]
    fn scandic_two_part_combined_exact_match() {
        // "Häl.so" combined is "hälso" (lowered) — exact match
        let parts = FilteredParts::new("Häl.so.Video.001.mp4");
        assert!(parts.prefix_matches_normalized("hälso"));
    }

    #[test]
    fn scandic_two_part_combined_with_uppercase_boundary() {
        // "Häl.soCenter" combined is "HälsoCenter", starts with "hälso" at uppercase 'C'
        let parts = FilteredParts::new("Häl.soCenter.Video.001.mp4");
        assert!(parts.prefix_matches_normalized("hälso"));
    }

    #[test]
    fn scandic_two_part_combined_with_lowercase_rejected() {
        // "Häl.sosam" combined is "Hälsosam", starts with "hälso" but 's' is lowercase
        let parts = FilteredParts::new("Häl.sosam.Video.001.mp4");
        assert!(!parts.prefix_matches_normalized("hälso"));
    }

    #[test]
    fn umlaut_prefix_not_confused_with_ascii() {
        // "Ställe" should not match "stalle" (different characters)
        let parts = FilteredParts::new("Ställe.Video.001.mp4");
        assert!(!parts.prefix_matches_normalized("stalle"));
    }
}
