//! The one rule set for linking money to bills.
//!
//! Payment matching used to live in four slightly different copies —
//! `payment_event`'s appointment and order paths, `collect_payment`, and
//! `book_appointment`'s pre-payment check — and they diverged exactly the way
//! duplicated money rules always do: the webhook's sole-pending shortcut
//! counted pending appointments but not pending orders, and it applied even
//! when the payer WAS named but matched nobody, so Carlos's Yape could close
//! Ana's only open booking. Both directions of the match now route through
//! here, under one set of rules:
//!
//! 1. **Identity first.** A named payer links only through
//!    [`crate::harness::best_name_match`] / [`name_matches`] — never by
//!    recency or FIFO.
//! 2. **A named payer that matches nobody never falls through to guesses.**
//! 3. **A nameless payment links only when the business has exactly one open
//!    bill anywhere** — appointments and orders counted together.
//! 4. **The amount must cover what the bill owes.** An unknowable owed amount
//!    (`None`) never auto-confirms; unverifiable money stays unlinked.

use crate::harness::{best_name_match, name_matches};

/// One open bill, whichever table it lives in: (id, customer name, owed).
/// `owed: None` means nothing is configured to verify against.
pub type Bill<Id> = (Id, String, Option<f64>);

fn covers(amount: Option<f64>, owed: Option<f64>) -> bool {
    matches!((amount, owed), (Some(paid), Some(o)) if paid >= o)
}

/// Webhook direction: a payment arrived — which bill does it settle?
///
/// `bills` is one table's open bills (the caller tries appointments first,
/// then orders); `total_open_bills` is the business-wide count across BOTH
/// tables, so the nameless shortcut can't fire while another table still has
/// an open bill the money might belong to.
pub fn pick_bill_for_payment<Id: Copy + PartialEq>(
    payer: Option<&str>,
    amount: Option<f64>,
    bills: &[Bill<Id>],
    total_open_bills: usize,
) -> Option<Id> {
    match payer {
        Some(p) => {
            let names: Vec<(Id, String)> = bills.iter().map(|b| (b.0, b.1.clone())).collect();
            best_name_match(p, &names)
                .filter(|id| bills.iter().find(|b| b.0 == *id).is_some_and(|b| covers(amount, b.2)))
        }
        None => (total_open_bills == 1 && bills.len() == 1)
            .then(|| &bills[0])
            .filter(|b| covers(amount, b.2))
            .map(|b| b.0),
    }
}

/// What `pick_payment_for_bill` found.
pub struct PaymentPick<Id> {
    /// The payment that settles the bill, if any.
    pub payment: Option<Id>,
    /// A payment of sufficient amount arrived, but under a name that does not
    /// match this customer — worth telling them to check the account name.
    pub named_mismatch: bool,
}

/// Tool direction: a customer claims they paid a known bill — which unlinked
/// payment proves it?
///
/// `candidates` are unlinked payments already filtered to `amount >= owed` by
/// the caller's query; `total_open_bills` is again the business-wide count, so
/// a nameless payment only counts when this bill is the only one it could be.
pub fn pick_payment_for_bill<Id: Copy>(
    candidates: &[(Id, Option<f64>, Option<String>)],
    customer_name: &str,
    total_open_bills: usize,
) -> PaymentPick<Id> {
    let named_mismatch = candidates.iter().any(|(_, _, payer)| {
        payer.as_deref().is_some_and(|p| !name_matches(p, customer_name))
    });
    let payment = candidates
        .iter()
        .find(|(_, _, payer)| {
            payer.as_deref().is_some_and(|p| name_matches(p, customer_name))
        })
        .or_else(|| {
            candidates
                .iter()
                .find(|(_, _, payer)| payer.is_none() && total_open_bills <= 1)
        })
        .map(|(id, _, _)| *id);
    PaymentPick { payment, named_mismatch }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bill(id: i32, name: &str, owed: Option<f64>) -> Bill<i32> {
        (id, name.to_string(), owed)
    }

    // ---------------------------------------------- webhook: payment → bill

    #[test]
    fn named_payer_links_by_name_and_amount() {
        let bills = vec![bill(1, "Ana Rojas", Some(50.0)), bill(2, "Rosa Quispe", Some(30.0))];
        assert_eq!(pick_bill_for_payment(Some("Ana R."), Some(50.0), &bills, 2), Some(1));
        // Enough money, wrong bill: the amount covers Rosa's 30 but Ana's name won.
        assert_eq!(pick_bill_for_payment(Some("Rosa Q."), Some(30.0), &bills, 2), Some(2));
        // Name matches, amount doesn't cover.
        assert_eq!(pick_bill_for_payment(Some("Ana R."), Some(10.0), &bills, 2), None);
    }

    /// The payment_event divergence this module exists to kill: a NAMED payer
    /// matching nobody must never fall through to the sole-pending shortcut.
    #[test]
    fn named_mismatch_never_falls_through_to_sole_pending() {
        let bills = vec![bill(1, "Ana Rojas", Some(50.0))];
        assert_eq!(pick_bill_for_payment(Some("Carlos Mendoza"), Some(50.0), &bills, 1), None);
    }

    /// The second divergence: sole-pending is business-wide. One pending
    /// appointment plus pending orders elsewhere = not sole.
    #[test]
    fn nameless_sole_pending_counts_both_tables() {
        let bills = vec![bill(1, "Ana Rojas", Some(50.0))];
        assert_eq!(pick_bill_for_payment(None, Some(50.0), &bills, 1), Some(1));
        // Same single appointment, but the business also has open orders.
        assert_eq!(pick_bill_for_payment(None, Some(50.0), &bills, 4), None);
    }

    #[test]
    fn unverifiable_owed_never_confirms() {
        let bills = vec![bill(1, "Ana Rojas", None)];
        assert_eq!(pick_bill_for_payment(Some("Ana Rojas"), Some(999.0), &bills, 1), None);
        assert_eq!(pick_bill_for_payment(None, Some(999.0), &bills, 1), None);
    }

    #[test]
    fn unparsed_amount_never_confirms() {
        let bills = vec![bill(1, "Ana Rojas", Some(50.0))];
        assert_eq!(pick_bill_for_payment(Some("Ana Rojas"), None, &bills, 1), None);
    }

    // ------------------------------------------------ tool: bill → payment

    #[test]
    fn named_payment_wins_over_nameless() {
        let pays = vec![
            (10, Some(50.0), None),
            (11, Some(50.0), Some("Ana Rojas".to_string())),
        ];
        let pick = pick_payment_for_bill(&pays, "Ana Rojas", 3);
        assert_eq!(pick.payment, Some(11));
        assert!(!pick.named_mismatch);
    }

    #[test]
    fn nameless_only_counts_when_bill_is_the_only_one() {
        let pays = vec![(10, Some(50.0), None)];
        assert_eq!(pick_payment_for_bill(&pays, "Ana Rojas", 1).payment, Some(10));
        assert_eq!(pick_payment_for_bill(&pays, "Ana Rojas", 2).payment, None);
    }

    #[test]
    fn wrong_name_reports_mismatch_instead_of_linking() {
        let pays = vec![(10, Some(50.0), Some("Carlos Mendoza".to_string()))];
        let pick = pick_payment_for_bill(&pays, "Ana Rojas", 1);
        assert_eq!(pick.payment, None);
        assert!(pick.named_mismatch);
    }

    // ------------------------------------------------------------ covers

    #[test]
    fn covers_needs_both_amounts_and_paid_at_least_owed() {
        assert!(covers(Some(50.0), Some(50.0)));
        assert!(covers(Some(50.01), Some(50.0)));
        assert!(!covers(Some(49.99), Some(50.0)));
        assert!(!covers(None, Some(50.0)));
        assert!(!covers(Some(50.0), None));
        assert!(!covers(None, None));
        // A free bill is covered by any known amount, including zero.
        assert!(covers(Some(0.0), Some(0.0)));
    }

    #[test]
    fn overpayment_still_settles() {
        let bills = vec![bill(1, "Ana Rojas", Some(50.0))];
        assert_eq!(pick_bill_for_payment(Some("Ana Rojas"), Some(80.0), &bills, 1), Some(1));
        assert_eq!(pick_bill_for_payment(None, Some(80.0), &bills, 1), Some(1));
    }

    #[test]
    fn empty_bill_list_links_nothing() {
        let bills: Vec<Bill<i32>> = vec![];
        assert_eq!(pick_bill_for_payment(Some("Ana"), Some(1.0), &bills, 0), None);
        assert_eq!(pick_bill_for_payment(None, Some(1.0), &bills, 0), None);
        // total says one open bill, but it lives in the other table.
        assert_eq!(pick_bill_for_payment(None, Some(1.0), &bills, 1), None);
    }

    #[test]
    fn nameless_with_two_bills_in_this_table_links_nothing() {
        let bills = vec![bill(1, "Ana Rojas", Some(50.0)), bill(2, "Rosa Quispe", Some(50.0))];
        assert_eq!(pick_bill_for_payment(None, Some(50.0), &bills, 2), None);
    }

    #[test]
    fn ambiguous_names_link_nothing() {
        // Two open bills for two different Anas: a wrong link is worse than none.
        let bills = vec![bill(1, "Ana Rojas", Some(50.0)), bill(2, "Ana Rojas", Some(50.0))];
        assert_eq!(pick_bill_for_payment(Some("Ana Rojas"), Some(50.0), &bills, 2), None);
    }

    #[test]
    fn short_name_tokens_do_not_match() {
        // Tokens under 3 letters are too common to prove identity.
        let bills = vec![bill(1, "Li Wu", Some(10.0))];
        assert_eq!(pick_bill_for_payment(Some("Li Wu"), Some(10.0), &bills, 1), None);
    }

    #[test]
    fn best_match_that_cannot_be_covered_is_not_replaced_by_another() {
        // Ana owes 50 and paid 30; Rosa owes 20. The money is Ana's — never Rosa's.
        let bills = vec![bill(1, "Ana Rojas", Some(50.0)), bill(2, "Rosa Quispe", Some(20.0))];
        assert_eq!(pick_bill_for_payment(Some("Ana Rojas"), Some(30.0), &bills, 2), None);
    }

    #[test]
    fn works_with_any_copy_id_type() {
        let bills: Vec<Bill<&str>> = vec![("appt-9", "Ana Rojas".into(), Some(5.0))];
        assert_eq!(pick_bill_for_payment(Some("Ana"), Some(5.0), &bills, 1), Some("appt-9"));
    }

    #[test]
    fn no_candidates_no_payment_no_mismatch() {
        let pays: Vec<(i32, Option<f64>, Option<String>)> = vec![];
        let pick = pick_payment_for_bill(&pays, "Ana Rojas", 1);
        assert_eq!(pick.payment, None);
        assert!(!pick.named_mismatch);
    }

    #[test]
    fn nameless_with_zero_open_bills_counted_still_links() {
        // total_open_bills <= 1: the caller's count may already exclude this bill.
        let pays = vec![(10, Some(5.0), None)];
        assert_eq!(pick_payment_for_bill(&pays, "Ana Rojas", 0).payment, Some(10));
    }

    #[test]
    fn match_found_alongside_mismatch_reports_both() {
        let pays = vec![
            (10, Some(50.0), Some("Carlos Mendoza".to_string())),
            (11, Some(50.0), Some("Ana Rojas".to_string())),
        ];
        let pick = pick_payment_for_bill(&pays, "Ana Rojas", 5);
        assert_eq!(pick.payment, Some(11));
        assert!(pick.named_mismatch);
    }

    #[test]
    fn first_matching_named_payment_wins() {
        let pays = vec![
            (10, Some(50.0), Some("Ana Rojas".to_string())),
            (11, Some(60.0), Some("Ana R".to_string())),
        ];
        assert_eq!(pick_payment_for_bill(&pays, "Ana Rojas", 1).payment, Some(10));
    }
}
