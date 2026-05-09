//! Graph matching and the linear-assignment subproblem.
//!
//! * [`linear_sum_assignment`] / [`linear_sum_assignment_max`] — Hungarian / LAP.
//! * [`faq_graph_match`] / [`faq_graph_match_seeded`] — Fast Approximate QAP.
//!
//! The module is named `match_` because `match` is a Rust keyword.
//! graspologic's Python module is named `graspologic.match`.

pub mod faq;
pub mod lap;

pub use faq::{
    faq_graph_match, faq_graph_match_seeded, FaqError, FaqMatch, FaqOptions,
};
pub use lap::{
    assignment_cost, linear_sum_assignment, linear_sum_assignment_max, LapError,
    LapResult,
};
