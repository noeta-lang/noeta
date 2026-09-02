//! The argument→parameter rule, over the whole width of a supplied mask.
//!
//! [`noeta_bytecode::param_of_arg`] and [`noeta_bytecode::is_param_filled`] are the two directions
//! of one relation, and every call prologue in both backends reads them. The relation is what the
//! tests below assert: walking a mask's set bits in order must land on exactly the parameters the
//! mask reports filled, for every argument the call passes.

use noeta_bytecode::{is_param_filled, param_of_arg};

/// Walking a mask returns its set bits in ascending order, and nothing else.
fn params_of(mask: u64) -> Vec<usize> {
    (0..mask.count_ones() as usize)
        .map(|i| param_of_arg(i, Some(mask)))
        .collect()
}

#[test]
fn a_mask_walks_to_exactly_the_parameters_it_reports_filled() {
    for mask in [
        0b1u64,
        0b101,
        0b1011,
        1 << 62,
        (1 << 62) | 1,
        u64::MAX >> 1,
        u64::MAX,
        0xDEAD_BEEF_CAFE_F00D,
    ] {
        let walked = params_of(mask);
        let filled: Vec<usize> = (0..64)
            .filter(|p| is_param_filled(*p, walked.len(), Some(mask)))
            .collect();
        assert_eq!(walked, filled, "mask {mask:#x}");
        assert!(walked.windows(2).all(|w| w[0] < w[1]), "mask {mask:#x}");
    }
}

/// The widest mask a call can carry names parameter 63 with its last argument, and the walk to it
/// clears every one of the 63 bits below without running out.
#[test]
fn the_last_argument_of_a_full_mask_lands_on_parameter_63() {
    assert_eq!(param_of_arg(63, Some(u64::MAX)), 63);
    assert_eq!(param_of_arg(0, Some(u64::MAX)), 0);
    assert_eq!(param_of_arg(62, Some(u64::MAX)), 62);
}

/// Without a mask an argument fills the parameter at its own index, at any arity. This is the rule
/// a call that merely reorders its labels reads by, which is why it has to hold past 64: a dense
/// prefix carries no mask, so nothing about it is bounded by the mask's width.
#[test]
fn an_unmasked_call_binds_by_position_at_any_arity() {
    for i in [0usize, 63, 64, 65, 200] {
        assert_eq!(param_of_arg(i, None), i);
        assert!(is_param_filled(i, i + 1, None));
        assert!(!is_param_filled(i, i, None));
    }
}
