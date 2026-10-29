//! What a ceiling is written as, and what resolving one produces.
//!
//! See `docs/architecture.md` §§4 and 8. The
//! grammar is tested here, in the crate that owns it, rather than beside the
//! flag: it is one parser with two surfaces, and a table of spellings that
//! lived next to `--budget` would say nothing about the config key that
//! shares it.

use ego_core::ceiling::{Budget, Ceilings, tokens_reached};
use ego_core::cost::{Amount, CurrencyCode, Money};
use ego_core::state::Usage;

mod ceiling {
    use super::*;

    fn code(code: &str) -> CurrencyCode {
        CurrencyCode::new(code).expect("a valid test code")
    }

    fn money(decimal: &str, currency: &str) -> Budget {
        Budget::Money(Money::new(
            decimal.parse::<Amount>().expect("a valid test amount"),
            code(currency),
        ))
    }

    fn parse(spec: &str) -> Result<Budget, String> {
        spec.parse::<Budget>().map_err(|err| err.to_string())
    }

    // --- the grammar -----------------------------------------------------

    /// A bare number is refused rather than guessed. "budget 400" is either
    /// four hundred dollars or four hundred tokens depending on who wrote it,
    /// and picking one silently is how a run costs a hundred times what its
    /// author intended.
    #[test]
    fn a_budget_with_no_unit_is_refused_rather_than_guessed() {
        let refusal = parse("400").expect_err("no unit");
        assert!(refusal.contains("names no unit"), "{refusal}");
        assert!(refusal.contains("2.5$"), "the refusal shows the spellings");
        assert!(refusal.contains("10EUR"), "the refusal shows the spellings");
        assert!(refusal.contains("400k"), "the refusal shows the spellings");
    }

    #[test]
    fn both_spellings_of_a_dollar_ceiling_are_the_same_budget() {
        let expected = money("2.50", "USD");
        for spec in ["2.5$", "$2.5", " 2.50$ ", "2.50$"] {
            assert_eq!(parse(spec).expect("a budget"), expected, "{spec}");
        }
    }

    /// §8's addition: a decimal plus an ISO 4217 code. Without it the only
    /// currency anybody could write was USD, and a EUR-priced row had no
    /// ceiling that could bind it — the gate refuses a limit in the wrong
    /// currency rather than converting, so `10$` against a EUR row is not a
    /// smaller ceiling, it is no ceiling at all.
    #[test]
    fn a_ceiling_can_be_written_in_the_currency_it_binds() {
        assert_eq!(parse("10EUR").expect("a budget"), money("10", "EUR"));
        assert_eq!(parse("2.5USD").expect("a budget"), money("2.5", "USD"));
        assert_eq!(parse(" 0.75GBP ").expect("a budget"), money("0.75", "GBP"));
        assert_eq!(
            parse("2.5USD").expect("a budget"),
            parse("2.5$").expect("a budget"),
            "and `$` is exactly the USD spelling, not a second kind of ceiling"
        );
    }

    /// A code with no figure in front of it is a currency, not a ceiling.
    /// Refused rather than read as "some unspecified amount of euros".
    #[test]
    fn a_bare_currency_code_is_not_a_ceiling() {
        for spec in ["EUR", "USD", "$"] {
            assert!(parse(spec).is_err(), "`{spec}` names no figure");
        }
    }

    #[test]
    fn token_budgets_take_the_suffixes_people_write() {
        for (spec, tokens) in [
            ("400k", 400_000),
            ("400K", 400_000),
            ("2m", 2_000_000),
            ("2M", 2_000_000),
        ] {
            assert_eq!(
                parse(spec).expect("a budget"),
                Budget::Tokens(tokens),
                "{spec}"
            );
        }
    }

    /// The `k`/`m` scale is one letter and a currency code is three, so the
    /// two arms of the grammar cannot collide however they are cased.
    ///
    /// The second half is the deliberate consequence: the currency arm
    /// validates *shape*, not membership, so `400KKK` is four hundred of a
    /// currency nobody issues rather than four hundred thousand tokens. That
    /// is the safe direction. A vendored list of live ISO 4217 codes would go
    /// stale in a way nothing here could detect, and the mistake it would
    /// catch is caught anyway at the first reservation, where a limit in a
    /// currency the quote is not in is refused rather than converted.
    #[test]
    fn a_token_suffix_is_never_read_as_a_currency() {
        assert_eq!(parse("400K").expect("a budget"), Budget::Tokens(400_000));
        assert_eq!(
            parse("400KKK").expect("a budget"),
            money("400", "KKK"),
            "three uppercase letters are a currency, never a token scale"
        );
    }

    #[test]
    fn a_token_count_that_does_not_fit_is_refused_rather_than_wrapped() {
        let refusal = parse("99999999999999999999m").expect_err("it does not fit");
        assert!(
            refusal.contains("whole number") || refusal.contains("64 bits"),
            "{refusal}"
        );
    }

    /// The strict decimal parser is inherited, not re-implemented: one
    /// spelling per value, so a thousands separator is a refusal rather than
    /// a silent truncation to `1`.
    #[test]
    fn a_money_ceiling_inherits_the_strict_decimal_parser() {
        for spec in ["1,000$", "1e3$", "-5$", "+5USD"] {
            assert!(parse(spec).is_err(), "`{spec}` is not one decimal");
        }
    }

    /// What was written is quoted back, whitespace and all, so an operator
    /// can see what was read rather than what they meant.
    #[test]
    fn the_refusal_quotes_what_was_actually_written() {
        let refusal = parse("four hundred").expect_err("not a budget");
        assert!(refusal.contains("`four hundred`"), "{refusal}");
    }

    // --- resolution and the system clamp ---------------------------------

    /// The two fields are independent instruments, and a layer naming one
    /// does not erase the other. If they shared a slot, naming a token
    /// ceiling would be a way to drop the money ceiling a lower layer set.
    #[test]
    fn a_token_ceiling_and_a_money_ceiling_coexist() {
        let mut ceilings = Ceilings::default();
        ceilings.apply(&money("5", "USD"));
        ceilings.apply(&Budget::Tokens(400_000));

        assert_eq!(
            ceilings,
            Ceilings {
                money: Some(Money::new(
                    "5".parse::<Amount>().expect("an amount"),
                    code("USD")
                )),
                tokens: Some(400_000),
            }
        );
    }

    /// Nothing is imposed by default. `CLI.md:296-299` — "ego itself imposes
    /// none" — so the resolved ceilings of a run nobody configured are two
    /// absences, not two large numbers.
    #[test]
    fn nothing_is_a_ceiling_until_somebody_writes_one() {
        assert_eq!(
            Ceilings::default(),
            Ceilings {
                money: None,
                tokens: None
            }
        );
    }

    /// The clamp is `min`, not "the administrator's figure wins". An
    /// operator who asked for less than the cap keeps their own — the cap
    /// binds everyone to a maximum, not to a value.
    #[test]
    fn a_system_ceiling_reduces_a_larger_request_and_leaves_a_smaller_one() {
        let cap = Ceilings {
            money: Some(Money::new(
                "5".parse::<Amount>().expect("an amount"),
                code("USD"),
            )),
            tokens: Some(100_000),
        };

        let mut greedy = Ceilings::default();
        greedy.apply(&money("20", "USD"));
        greedy.apply(&Budget::Tokens(400_000));
        let clamped = greedy.clamp(&cap);
        assert_eq!(greedy, cap, "both fields were reduced to the cap");
        assert_eq!(
            clamped.money,
            Some(Money::new(
                "20".parse::<Amount>().expect("an amount"),
                code("USD")
            )),
            "and the figure that was asked for is reported, not swallowed"
        );
        assert_eq!(clamped.tokens, Some(400_000));
        assert!(clamped.any());

        let mut modest = Ceilings::default();
        modest.apply(&money("1", "USD"));
        modest.apply(&Budget::Tokens(1_000));
        let untouched = modest.clamp(&cap);
        assert_eq!(
            modest.money,
            Some(Money::new(
                "1".parse::<Amount>().expect("an amount"),
                code("USD")
            ))
        );
        assert_eq!(modest.tokens, Some(1_000));
        assert!(
            !untouched.any(),
            "asking for less than the cap is not a clamp, and must not be reported as one"
        );
    }

    /// A cap on one field applies even when nothing was asked for on it: an
    /// administrator's ceiling is in force whether or not the operator
    /// mentioned it.
    #[test]
    fn a_system_ceiling_applies_where_nothing_was_asked_for() {
        let cap = Ceilings {
            money: Some(Money::new(
                "5".parse::<Amount>().expect("an amount"),
                code("USD"),
            )),
            tokens: Some(100_000),
        };
        let mut nothing = Ceilings::default();
        let clamped = nothing.clamp(&cap);

        assert_eq!(nothing, cap);
        assert!(
            !clamped.any(),
            "and nothing was reduced, because nothing was asked for"
        );
    }

    /// The escape the two-field clamp exists to close: a user must not slip
    /// an administrator's money cap by naming a token ceiling instead. The
    /// fields clamp independently, so an untouched field is still capped.
    #[test]
    fn naming_one_ceiling_does_not_escape_the_cap_on_the_other() {
        let cap = Ceilings {
            money: Some(Money::new(
                "5".parse::<Amount>().expect("an amount"),
                code("USD"),
            )),
            tokens: Some(100_000),
        };
        let mut evasive = Ceilings::default();
        evasive.apply(&Budget::Tokens(10_000_000));
        evasive.clamp(&cap);

        assert_eq!(evasive.tokens, Some(100_000), "the named field is capped");
        assert_eq!(
            evasive.money, cap.money,
            "and so is the one that was carefully not named"
        );
    }

    /// A cap in a currency the request is not in cannot be compared, so it
    /// replaces outright rather than letting the request through. Converting
    /// would need a rate, and a rate would make an administrator's cap depend
    /// on a table they did not write.
    #[test]
    fn a_cap_in_another_currency_replaces_rather_than_converts() {
        let cap = Ceilings {
            money: Some(Money::new(
                "5".parse::<Amount>().expect("an amount"),
                code("EUR"),
            )),
            tokens: None,
        };
        let mut asked = Ceilings::default();
        asked.apply(&money("1", "USD"));
        let clamped = asked.clamp(&cap);

        assert_eq!(asked.money, cap.money);
        assert_eq!(
            clamped.money,
            Some(Money::new(
                "1".parse::<Amount>().expect("an amount"),
                code("USD")
            )),
            "and the replacement is stated, because $1 is not €5"
        );
    }

    /// §5's gate, and §9's claim that the token ceiling is the one instrument
    /// that needs no rates, no fetch and no network.
    ///
    /// The middle case is the whole test. A token ceiling is still a ceiling,
    /// so a gate written as "any ceiling at all" would pass every other
    /// assertion here and quietly drag a price lookup in behind it — which is
    /// exactly what an air-gapped operator reached for the token ceiling to
    /// avoid, and what §7's first row promises does not happen.
    #[test]
    fn only_a_money_ceiling_needs_rates() {
        assert!(
            !Ceilings::default().needs_rates(),
            "no ceiling consults nothing, which is §5's ordinary case"
        );

        let mut tokens_only = Ceilings::default();
        tokens_only.apply(&Budget::Tokens(400_000));
        assert!(
            !tokens_only.needs_rates(),
            "a token ceiling is checked against token counts the session \
             already has, so it needs no price and must not ask for one"
        );

        let mut with_money = Ceilings::default();
        with_money.apply(&money("5", "USD"));
        assert!(with_money.needs_rates());

        with_money.apply(&Budget::Tokens(400_000));
        assert!(
            with_money.needs_rates(),
            "and adding a token ceiling beside a money one does not excuse the \
             money one from needing rates"
        );
    }

    // --- the token check --------------------------------------------------

    fn used(total: u64) -> Usage {
        Usage {
            uncached_input_tokens: total,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            cache_write_extended_tokens: 0,
            output_tokens: 0,
            web_search_requests: 0,
            reasoning_tokens: None,
        }
    }

    /// "Reached", not "exceeded": `CLI.md:128` says exit 5 when the ceiling
    /// is hit, so landing exactly on it stops the run.
    #[test]
    fn a_token_ceiling_is_reached_at_the_figure_not_past_it() {
        assert!(!tokens_reached(Some(1_000), &used(999)));
        assert!(tokens_reached(Some(1_000), &used(1_000)));
        assert!(tokens_reached(Some(1_000), &used(1_001)));
    }

    #[test]
    fn no_token_ceiling_is_never_reached() {
        assert!(!tokens_reached(None, &used(u64::MAX)));
    }
}
