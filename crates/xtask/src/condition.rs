//! Boolean condition expressions used by task `when` filters and named `[conditions]`.
//!
//! An expression combines identifiers with `!`, `&&`, `||`, and parentheses,
//! for example `"sign-in && (ios || !android)"`.
//! Identifiers name task file flags, other named conditions,
//! or the built-in conditions in [`BUILTIN_CONDITIONS`].
//! `!` binds tightest, then `&&`, then `||`.

use anyhow::{Result, bail};

/// Condition names that are always available, evaluated from the platform and the run options.
pub const BUILTIN_CONDITIONS: &[&str] = &["macos", "linux", "windows", "unix", "ci", "dryrun"];

/// Most tokens accepted in one expression.
///
/// Parsing, evaluating, and dropping a condition all recurse once per level of the tree,
/// and a tree is never deeper than its token count,
/// so the cap turns a runaway expression into an error instead of a stack overflow.
const MAX_TOKENS: usize = 1024;

/// Parsed boolean condition expression.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Condition {
    /// A flag, named condition, or built-in condition.
    Identifier(String),
    /// Logical negation.
    Not(Box<Self>),
    /// Both sides must hold.
    And(Box<Self>, Box<Self>),
    /// At least one side must hold.
    Or(Box<Self>, Box<Self>),
}

/// Lexical token of a condition expression.
#[derive(Clone, Debug, Eq, PartialEq)]
enum Token {
    Identifier(String),
    Not,
    And,
    Or,
    Open,
    Close,
}

/// Recursive descent parser over the token list.
struct Parser {
    tokens: Vec<Token>,
    position: usize,
}

impl Condition {
    /// Parse an expression such as `"all && !coverage"`.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty expression, an unexpected character or token, or unbalanced parentheses.
    pub fn parse(expression: &str) -> Result<Self> {
        let tokens = tokenize(expression)?;
        if tokens.is_empty() {
            bail!("Empty condition expression");
        }
        if tokens.len() > MAX_TOKENS {
            bail!("Condition has more than {MAX_TOKENS} tokens");
        }
        let mut parser = Parser { tokens, position: 0 };
        let condition = parser.parse_or()?;
        if let Some(token) = parser.peek() {
            bail!("Unexpected {token:?} in condition '{expression}'");
        }
        Ok(condition)
    }

    /// Every identifier the expression refers to, in order of appearance.
    #[must_use]
    pub fn identifiers(&self) -> Vec<&str> {
        let mut names = Vec::new();
        self.collect_identifiers(&mut names);
        names
    }

    /// Evaluate the expression, resolving each identifier with `resolve`.
    ///
    /// # Errors
    ///
    /// Returns the first error produced by `resolve`.
    pub fn evaluate<F>(&self, resolve: &mut F) -> Result<bool>
    where
        F: FnMut(&str) -> Result<bool>,
    {
        match self {
            Self::Identifier(name) => resolve(name),
            Self::Not(inner) => Ok(!inner.evaluate(resolve)?),
            Self::And(left, right) => Ok(left.evaluate(resolve)? && right.evaluate(resolve)?),
            Self::Or(left, right) => Ok(left.evaluate(resolve)? || right.evaluate(resolve)?),
        }
    }

    fn collect_identifiers<'a>(&'a self, names: &mut Vec<&'a str>) {
        match self {
            Self::Identifier(name) => names.push(name),
            Self::Not(inner) => inner.collect_identifiers(names),
            Self::And(left, right) | Self::Or(left, right) => {
                left.collect_identifiers(names);
                right.collect_identifiers(names);
            }
        }
    }
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.position)
    }

    fn advance(&mut self) -> Option<Token> {
        let token = self.tokens.get(self.position).cloned();
        self.position = self.position.saturating_add(1);
        token
    }

    fn parse_or(&mut self) -> Result<Condition> {
        let mut left = self.parse_and()?;
        while self.peek() == Some(&Token::Or) {
            self.advance();
            let right = self.parse_and()?;
            left = Condition::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<Condition> {
        let mut left = self.parse_unary()?;
        while self.peek() == Some(&Token::And) {
            self.advance();
            let right = self.parse_unary()?;
            left = Condition::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_unary(&mut self) -> Result<Condition> {
        match self.advance() {
            Some(Token::Not) => Ok(Condition::Not(Box::new(self.parse_unary()?))),
            Some(Token::Open) => {
                let inner = self.parse_or()?;
                if self.advance() != Some(Token::Close) {
                    bail!("Missing closing parenthesis in condition");
                }
                Ok(inner)
            }
            Some(Token::Identifier(name)) => Ok(Condition::Identifier(name)),
            Some(token) => bail!("Unexpected {token:?} in condition"),
            None => bail!("Condition ends unexpectedly"),
        }
    }
}

/// Split an expression into tokens.
fn tokenize(expression: &str) -> Result<Vec<Token>> {
    let mut tokens = Vec::new();
    let mut characters = expression.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            ' ' | '\t' | '\n' | '\r' => {}
            '!' => tokens.push(Token::Not),
            '(' => tokens.push(Token::Open),
            ')' => tokens.push(Token::Close),
            '&' | '|' => {
                if characters.next() != Some(character) {
                    bail!("Expected '{character}{character}' in condition '{expression}'");
                }
                tokens.push(if character == '&' { Token::And } else { Token::Or });
            }
            _ if is_identifier_character(character) => {
                let mut name = String::from(character);
                while let Some(&next) = characters.peek() {
                    if !is_identifier_character(next) {
                        break;
                    }
                    name.push(next);
                    characters.next();
                }
                tokens.push(Token::Identifier(name));
            }
            _ => bail!("Unexpected character '{character}' in condition '{expression}'"),
        }
    }
    Ok(tokens)
}

/// Return whether `character` is allowed in a condition identifier.
const fn is_identifier_character(character: char) -> bool {
    character.is_ascii_alphanumeric() || character == '-' || character == '_'
}

#[cfg(test)]
mod test_condition_parsing {
    use super::*;

    fn evaluate(expression: &str, true_names: &[&str]) -> bool {
        Condition::parse(expression)
            .expect("expression should parse")
            .evaluate(&mut |name| Ok(true_names.contains(&name)))
            .expect("evaluation should succeed")
    }

    #[test]
    fn parses_single_identifier_with_dashes() {
        assert_eq!(
            Condition::parse("sign-in").expect("should parse"),
            Condition::Identifier("sign-in".to_owned())
        );
    }

    #[test]
    fn applies_operator_precedence() {
        assert!(evaluate("a || b && c", &["a"]));
        assert!(!evaluate("a || b && c", &["b"]));
        assert!(evaluate("(a || b) && c", &["b", "c"]));
        assert!(evaluate("!a && b", &["b"]));
        assert!(!evaluate("!(a || b)", &["b"]));
    }

    #[test]
    fn evaluates_platform_dependent_sign_in_selection() {
        let ios = "ios || (sign-in && !android && macos)";
        assert!(evaluate(ios, &["sign-in", "macos"]));
        assert!(!evaluate(ios, &["sign-in", "android", "macos"]));
        assert!(evaluate(ios, &["ios"]));
        let android = "android || (sign-in && !ios && (all || !macos))";
        assert!(evaluate(android, &["sign-in", "all", "macos"]));
        assert!(!evaluate(android, &["sign-in", "macos"]));
        assert!(evaluate(android, &["sign-in"]));
    }

    #[test]
    fn lists_identifiers_in_order() {
        let condition = Condition::parse("a && (b || !c)").expect("should parse");
        assert_eq!(condition.identifiers(), ["a", "b", "c"]);
    }

    #[test]
    fn rejects_malformed_expressions() {
        for expression in ["", "a &", "a && ", "(a", "a)", "a b", "a & b", "a == b", "!"] {
            assert!(Condition::parse(expression).is_err(), "{expression:?} should not parse");
        }
    }

    #[test]
    fn propagates_resolver_errors() {
        let condition = Condition::parse("unknown").expect("should parse");
        assert!(condition.evaluate(&mut |_| bail!("unknown identifier")).is_err());
    }
}

#[cfg(test)]
mod test_condition_properties {
    use proptest::prelude::*;

    use super::*;

    /// Small identifier set, so expressions repeat names and the truth table stays small.
    const NAMES: &[&str] = &["ci", "macos", "sign-in", "fast_path", "a1"];

    /// Generated condition tree.
    fn condition() -> impl Strategy<Value = Condition> {
        let leaf = prop::sample::select(NAMES).prop_map(|name| Condition::Identifier(name.to_string()));
        leaf.prop_recursive(6, 48, 2, |inner| {
            prop_oneof![
                inner.clone().prop_map(|condition| Condition::Not(Box::new(condition))),
                (inner.clone(), inner.clone())
                    .prop_map(|(left, right)| Condition::And(Box::new(left), Box::new(right))),
                (inner.clone(), inner).prop_map(|(left, right)| Condition::Or(Box::new(left), Box::new(right))),
            ]
        })
    }

    /// Render with every binary operation in parentheses, so parsing must rebuild the same tree.
    fn render(condition: &Condition) -> String {
        match condition {
            Condition::Identifier(name) => name.clone(),
            Condition::Not(inner) => format!("!{}", render(inner)),
            Condition::And(left, right) => format!("({} && {})", render(left), render(right)),
            Condition::Or(left, right) => format!("({} || {})", render(left), render(right)),
        }
    }

    /// Render without parentheses where precedence makes them unnecessary.
    fn render_minimal(condition: &Condition) -> String {
        match condition {
            Condition::Identifier(name) => name.clone(),
            Condition::Not(inner) => match inner.as_ref() {
                Condition::And(..) | Condition::Or(..) => format!("!({})", render_minimal(inner)),
                _ => format!("!{}", render_minimal(inner)),
            },
            Condition::And(left, right) => {
                let wrap = |side: &Condition, parenthesize_and: bool| match side {
                    Condition::Or(..) => format!("({})", render_minimal(side)),
                    Condition::And(..) if parenthesize_and => format!("({})", render_minimal(side)),
                    _ => render_minimal(side),
                };
                format!("{} && {}", wrap(left, false), wrap(right, true))
            }
            Condition::Or(left, right) => {
                let right_text = match right.as_ref() {
                    Condition::Or(..) => format!("({})", render_minimal(right)),
                    _ => render_minimal(right),
                };
                format!("{} || {}", render_minimal(left), right_text)
            }
        }
    }

    /// Evaluate with the names set to true by the bits of `assignment`.
    fn truth(condition: &Condition, assignment: u8) -> bool {
        condition
            .evaluate(&mut |name: &str| {
                let index = NAMES.iter().position(|known| *known == name).unwrap_or_default();
                Ok(assignment & (1 << index) != 0)
            })
            .expect("resolving a known name never fails")
    }

    proptest! {
        #[test]
        fn parsing_a_rendered_condition_rebuilds_it(condition in condition()) {
            let parsed = Condition::parse(&render(&condition)).expect("a rendered condition should parse");
            prop_assert_eq!(parsed, condition);
        }

        #[test]
        fn minimal_parentheses_keep_the_same_tree(condition in condition()) {
            let text = render_minimal(&condition);
            let parsed = Condition::parse(&text).expect("a rendered condition should parse");
            prop_assert_eq!(parsed, condition, "{}", text);
        }

        #[test]
        fn identifiers_are_listed_in_order(condition in condition()) {
            let text = render(&condition);
            let expected: Vec<&str> = text
                .split(|character: char| !is_identifier_character(character))
                .filter(|part| !part.is_empty())
                .collect();
            prop_assert_eq!(condition.identifiers(), expected);
        }

        #[test]
        fn rendering_preserves_the_truth_table(condition in condition()) {
            let parsed = Condition::parse(&render_minimal(&condition)).expect("a rendered condition should parse");
            for assignment in 0..32_u8 {
                prop_assert_eq!(truth(&parsed, assignment), truth(&condition, assignment));
            }
        }

        #[test]
        fn parsing_never_panics(text in "[a-z!&|() \\-_]{0,60}") {
            let _ = Condition::parse(&text);
        }
    }

    #[test]
    fn deeply_nested_expressions_are_an_error_instead_of_a_stack_overflow() {
        let negations = format!("{}ci", "!".repeat(100_000));
        assert!(Condition::parse(&negations).is_err());
        let parentheses = format!("{}ci{}", "(".repeat(100_000), ")".repeat(100_000));
        assert!(Condition::parse(&parentheses).is_err());
    }

    #[test]
    fn long_flat_chains_are_an_error_instead_of_a_stack_overflow() {
        let chain = vec!["ci"; 100_000].join(" && ");
        assert!(Condition::parse(&chain).is_err());
    }

    #[test]
    fn the_longest_accepted_chain_parses_evaluates_and_drops() {
        let chain = vec!["ci"; MAX_TOKENS.div_ceil(2)].join(" && ");
        let condition = Condition::parse(&chain).expect("a chain within the limit should parse");
        assert!(
            condition
                .evaluate(&mut |_| Ok(true))
                .expect("evaluation should succeed")
        );

        let negations = format!("{}ci", "!".repeat(MAX_TOKENS - 1));
        let condition = Condition::parse(&negations).expect("nesting within the limit should parse");
        assert!(
            !condition
                .evaluate(&mut |_| Ok(true))
                .expect("evaluation should succeed")
        );

        let half = (MAX_TOKENS - 1) / 2;
        let parentheses = format!("{}ci{}", "(".repeat(half), ")".repeat(half));
        assert!(Condition::parse(&parentheses).is_ok());
    }
}
